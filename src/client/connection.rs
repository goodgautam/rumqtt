use std::cell::RefCell;
use std::rc::Rc;
use std::net::SocketAddr;
use std::net::ToSocketAddrs;
use std::time::Duration;
use std::path::Path;
use std::io::{self, ErrorKind};

use futures::{future, Future, Sink};
use futures::stream::{self, Stream, SplitStream};
use futures::sync::mpsc::Receiver;
use futures::unsync;
use futures::unsync::mpsc::UnboundedSender;
use futures::future::Either;
use tokio_core::reactor::{Core, Interval, Timeout};
use tokio_core::net::TcpStream;
use tokio_io::AsyncRead;
use tokio_io::codec::Framed;

use openssl::ssl::{SslMethod, SslConnector, SslVerifyMode, SslFiletype};
use tokio_openssl::SslConnectorExt;

use mqtt3::Packet;

use error::ConnectError;
use mqttopts::{MqttOptions, ConnectionMethod};
use client::state::MqttState;
use client::network::NetworkStream;
use client::{Command, ConnectCount};
use codec::MqttCodec;
use crossbeam_channel;

// DEVELOPER NOTES: Don't use `wait` in eventloop thread even if you
//                  are ok with blocking code. It might cause deadlocks
// https://github.com/tokio-rs/tokio-core/issues/182


pub struct Connection {
    commands_rx: Receiver<Command>,
    notifier_tx: crossbeam_channel::Sender<Packet>,
    mqtt_state: Rc<RefCell<MqttState>>,
    connection_count: ConnectCount,
    opts: MqttOptions,
    reactor: Core,
}

impl Connection {
    pub fn new(opts: MqttOptions, commands_rx: Receiver<Command>, notifier_tx: crossbeam_channel::Sender<Packet>) -> Self {
        Connection {
            commands_rx: commands_rx,
            notifier_tx: notifier_tx,
            mqtt_state: Rc::new(RefCell::new(MqttState::new(opts.clone()))),
            connection_count: ConnectCount::InitialConnect,
            opts: opts,
            reactor: Core::new().expect("Unable to create new reactor")
        }
    }

    // TODO: This method is too big. Passing rx as reference to a method to create
    //       network sender future is not ergonomic. Check other ways of reusing rx
    //       in the loop and creating a sender future
    pub fn start(&mut self) -> Result<(), (ConnectError, ConnectCount)> {
        let framed = match self.mqtt_connect() {
            Ok(framed) => framed,
            Err(e) => return Err((e, self.connection_count))
        };
        
        info!("mqtt connection successful");
        self.connection_count = ConnectCount::ConnectedBefore(1);

        let (network_reply_tx, mut network_reply_rx) = unsync::mpsc::unbounded::<Command>();
        let (sender, receiver) = framed.split();
        let mqtt_recv = self.mqtt_network_recv_future(receiver, network_reply_tx.clone());
        let ping_timer = self.ping_timer_future(network_reply_tx.clone());

        let last_session_publishes = self.mqtt_state.borrow_mut().handle_reconnection().into_iter().map(Command::Mqtt);
        let mut last_session_publishes = stream::iter_ok::<_, ()>(last_session_publishes);
        let last_session_publishes = last_session_publishes.by_ref();

        // receive incoming user request and write to network
        let mqtt_state = self.mqtt_state.clone();

        let commands_rx = self.commands_rx.by_ref();
        let network_reply_rx = network_reply_rx.by_ref();

        let mqtt_send = last_session_publishes
                        .chain(commands_rx)
                        .select(network_reply_rx)
                        .map_err(|e| {
                            error!("Receving outgoing message failed. Error = {:?}", e);
                            ConnectError::Outgoing
                        })
                        .and_then(|msg| {
                            match msg {
                                Command::Mqtt(packet) => match mqtt_state.borrow_mut().handle_outgoing_mqtt_packet(packet) {
                                    Ok(packet) => {
                                        debug!("Sending packet. {}", packet_info(&packet));
                                        future::ok(packet)
                                    }
                                    Err(e) => {
                                        error!("Handling outgoing packet failed. Error = {:?}", e);
                                        future::err(ConnectError::Outgoing)
                                    }
                                }
                                Command::Halt => future::err(ConnectError::Halt)
                            }
                        })
                        .forward(sender)
                        .map(|_| { mqtt_state.borrow_mut().reset_last_control_at();});
        
        // join mqtt send and ping timer. continues even if one of the stream ends
        let mqtt_send_and_ping = ping_timer.map_err(|e| ConnectError::Io(e)).join(mqtt_send).map(|_| ());
        
        // join all the futures and run the reactor
        let mqtt_send_and_recv = (mqtt_recv.map_err(|e| ConnectError::Io(e))).select(mqtt_send_and_ping);
        
        match self.reactor.run(mqtt_send_and_recv) {
            Ok((v, _next)) => {
                mqtt_state.borrow_mut().handle_disconnect();
                error!("Reactor stopped. v = {:?}", v);
                Ok(())
            }
            Err((e, _next)) => {
                mqtt_state.borrow_mut().handle_disconnect();
                error!("Reactor stopped. e = {:?}", e);
                Err((e, self.connection_count))
            }
        }
    }


    /// Receives incoming mqtt packets and forwards them appropriately to user and network
    //  TODO: Remove box when `impl Future` is stable
    //  NOTE: Uses `unbounded` channel for sending notifications back to network as bounded
    //        channel clone will double the size of the queue anyway.
    fn mqtt_network_recv_future(&self, receiver: SplitStream<Framed<NetworkStream, MqttCodec>>, network_reply_tx: UnboundedSender<Command>) -> Box<Future<Item=(), Error=io::Error>> {
        let mqtt_state = self.mqtt_state.clone();
        let notifier = self.notifier_tx.clone();
        
        let receiver = receiver.for_each(move |packet| {
            debug!("Received packet. {:?}", packet_info(&packet));
            let (notification, reply) = match mqtt_state.borrow_mut().handle_incoming_mqtt_packet(packet) {
                Ok((notification, reply)) => (notification, reply),
                Err(e) => {
                    error!("Incoming packet handle failed. Error = {:?}", e);
                    (None, None)
                }
            };

            // send notification to user
            if let Some(notification) = notification {
                if let Err(e) = notifier.try_send(notification) {
                    error!("Publish notification send failed. Error = {:?}", e);
                }
            }

            // send reply back to network
            let network_reply_tx = network_reply_tx.clone();
            if let Some(reply) = reply {
                let s = network_reply_tx.send(Command::Mqtt(reply)).map(|_| ()).map_err(|_| io::Error::new(ErrorKind::Other, "Error receiving client msg"));
                Box::new(s) as Box<Future<Item=(), Error=io::Error>>
            } else {
                Box::new(future::ok(()))
            }
        });

        Box::new(receiver)
    }

    /// Sends ping to the network when client is idle
    fn ping_timer_future(&self, network_reply_tx: UnboundedSender<Command>) -> Box<Future<Item=(), Error=io::Error>> {
        let handle = self.reactor.handle();
        let mqtt_state = self.mqtt_state.clone();

        if let Some(keep_alive) = self.opts.keep_alive {
            let interval = Interval::new(Duration::new(u64::from(keep_alive), 0), &handle).unwrap();
            let timer_future = interval.for_each(move |_t| {
                debug!("Ping timer fired. last flush = {:?}", mqtt_state.borrow_mut().last_flush);
                let network_reply_tx = network_reply_tx.clone();
                let s = network_reply_tx.send(Command::Mqtt(Packet::Pingreq)).map(|_| ()).map_err(|_| io::Error::new(ErrorKind::Other, "Error receiving client msg"));
                Box::new(s) as Box<Future<Item=(), Error=io::Error>>
            });

            Box::new(timer_future)
        } else {
            Box::new(future::ok(()))
        }
    }

    pub fn mqtt_connect(&mut self) -> Result<Framed<NetworkStream, MqttCodec>, ConnectError> {
        let stream = self.create_network_stream()?;
        let framed = stream.framed(MqttCodec);
        let connect = self.mqtt_state.borrow_mut().handle_outgoing_connect()?;
        let connect_timeout = Timeout::new(Duration::new(30, 0), &self.reactor.handle())?;
        
        let framed = framed.send(Packet::Connect(connect)).and_then(|framed| {
            framed.into_future().and_then(|(res, stream)| Ok((res, stream))).map_err(|(err, _stream)| err)
        });

        // this allows configuring connection timeout if the connection takes too long
        // and most importantly used to prevent blocking indefinitely while waiting for
        // connack packets. 
        // If a tcp halfopen connection happens right after tcp connection is
        // established and mqtt connect packet is sent, timeout prevents indefinite
        // waiting for connack packet
        let framed_with_timeout = framed.select2(connect_timeout);
        let framed_with_timeout = framed_with_timeout.map_err(|err| { 
            match err {
                Either::A((e, _)) => ConnectError::Io(e),
                Either::B((e, _)) => ConnectError::Io(e)
            }
        });

        let ((packet, framed), _timeout) = match self.reactor.run(framed_with_timeout)? {
            Either::A(f) => f,
            Either::B(_t) => return Err(ConnectError::Timeout)
        };
        
        match packet {
            Some(Packet::Connack(connack)) => {
                self.mqtt_state.borrow_mut().handle_incoming_connack(connack)?;
                Ok(framed)
            }
            None => Err(io::Error::new(ErrorKind::Other, "Connection closed by server").into()),
            _ => unimplemented!(),
        } 
    }

    fn create_network_stream(&mut self) -> Result<NetworkStream, ConnectError> {
        let (addr, domain) = self.get_socket_address()?;
        let connection_method = self.opts.connection_method.clone();
        let handle = self.reactor.handle();

        let tcp_future = TcpStream::connect(&addr, &handle).map(|tcp| tcp);

        let network_stream = match connection_method {
            ConnectionMethod::Tcp => {
                let network_future = tcp_future.map(move |connection| NetworkStream::Tcp(connection));
                self.reactor.run(network_future)?
            },
            ConnectionMethod::Tls(ca, client_pair) => {
                let connector = self.new_tls_connector(ca, client_pair, true)?;
          
                let tls_future = tcp_future.and_then(|tcp| {
                    let tls = connector.connect_async(&domain, tcp);
                    tls.map_err(|e| io::Error::new(io::ErrorKind::Other, e))
                });

                let network_future = tls_future.map(move |connection| NetworkStream::Tls(connection));
                self.reactor.run(network_future)?
            }
        };

        Ok(network_stream)
    }

    fn new_tls_connector<CA, C, K>(&self, ca: CA, client_pair: Option<(C, K)>, should_verify_ca: bool) -> Result<SslConnector, ConnectError>
    where
        CA: AsRef<Path>,
        C: AsRef<Path>,
        K: AsRef<Path>,
    {
        let mut tls_builder = SslConnector::builder(SslMethod::tls())?;
        tls_builder.set_ca_file(ca.as_ref())?;

        if let Some((cert, key)) = client_pair {
            tls_builder.set_certificate_file(cert, SslFiletype::PEM)?;
            tls_builder.set_private_key_file(key, SslFiletype::PEM)?;
        }

        tls_builder.set_verify(SslVerifyMode::NONE);

        Ok(tls_builder.build())
    }

    fn get_socket_address(&self) -> Result<(SocketAddr, String), ConnectError> {
        let addr = self.opts.broker_addr.clone();
        let domain = addr.split(":")
                         .map(str::to_string)
                         .next()
                         .unwrap_or_default();
        let addr = addr.to_socket_addrs()?.next();
        info!("Address resolved to {:?}", addr);

        match addr {
            Some(a) => Ok((a, domain)),
            None => return Err(ConnectError::DnsListEmpty),
        }
    }
}

fn packet_info(packet: &Packet) -> String {
    match *packet {
        Packet::Publish(ref p) => format!("topic = {}, qos = {:?}, pkid = {:?}, payload size = {:?} bytes", p.topic_name, p.qos, p.pid, p.payload.len()),
        _ => format!("{:?}", packet)
    }
}