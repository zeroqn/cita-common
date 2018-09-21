// CITA
// Copyright 2016-2018 Cryptape Technologies LLC.

// This program is free software: you can redistribute it
// and/or modify it under the terms of the GNU General Public
// License as published by the Free Software Foundation,
// either version 3 of the License, or (at your option) any
// later version.

// This program is distributed in the hope that it will be
// useful, but WITHOUT ANY WARRANTY; without even the implied
// warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR
// PURPOSE. See the GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use futures::future::Future;
use futures::stream;
use futures::sync::mpsc;
use futures::Stream;
use lapin::channel::{
    BasicConsumeOptions, BasicProperties, BasicPublishOptions, ExchangeDeclareOptions,
    QueueBindOptions, QueueDeclareOptions, QueuePurgeOptions,
};
use lapin::client::{Client, ConnectionOptions};
use lapin::message::Delivery;
use lapin::types::FieldTable;
use std::io;
use std::io::{Error, ErrorKind};
use std::net::SocketAddr;
use std::sync::mpsc as std_mpsc;
use std::thread;
use tokio;
use tokio::net::TcpStream;
use tokio::runtime::Runtime;
use tokio_io::{AsyncRead, AsyncWrite};

use super::{Payload, AMQP_URL, EXCHANGE, EXCHANGE_TYPE};

fn connect_consumer(
    addr: SocketAddr,
    name: &'static str,
    keys: Vec<String>,
    consumer_tx: std_mpsc::Sender<Payload>,
) -> impl Future<Item = (), Error = io::Error> + Send + 'static {
    TcpStream::connect(&addr)
        .and_then(|stream| Client::connect(stream, ConnectionOptions::default()))
        .and_then(move |(client, heartbeat)| {
            tokio::spawn(heartbeat.map_err(|_| ()));
            consumer(&client, name, keys, consumer_tx)
        })
}

fn connect_publisher(
    addr: SocketAddr,
    name: &'static str,
    publisher_rx: mpsc::Receiver<Payload>,
) -> impl Future<Item = (), Error = io::Error> + Send + 'static {
    TcpStream::connect(&addr)
        .and_then(|stream| Client::connect(stream, ConnectionOptions::default()))
        .and_then(move |(client, heartbeat)| {
            tokio::spawn(heartbeat.map_err(|_| ()));
            publisher(&client, name, publisher_rx)
        })
}

fn publisher<T>(
    client: &Client<T>,
    name: &'static str,
    publisher_rx: mpsc::Receiver<Payload>,
) -> impl Future<Item = (), Error = io::Error> + Send + 'static
where
    T: AsyncRead + AsyncWrite,
    T: Send + Sync,
    T: 'static,
{
    client.create_channel().and_then(move |channel| {
        channel
            .queue_declare(name, QueueDeclareOptions::default(), FieldTable::new())
            .and_then(move |_| {
                debug!("publisher queue declared");
                channel
                    .queue_purge(name, QueuePurgeOptions::default())
                    .and_then(move |_| {
                        debug!("publisher queue purged");
                        publisher_rx
                            .for_each(move |(routing_key, msg): (String, Vec<u8>)| {
                                tokio::spawn(
                                    channel
                                        .basic_publish(
                                            EXCHANGE,
                                            &routing_key,
                                            msg,
                                            BasicPublishOptions::default(),
                                            BasicProperties::default(),
                                        )
                                        .map(|_| ())
                                        .map_err(|_| ()),
                                )
                            })
                            .map_err(|_| Error::new(ErrorKind::Other, "channel closed!"))
                    })
            })
    })
}

fn consumer<T>(
    client: &Client<T>,
    name: &'static str,
    keys: Vec<String>,
    consumer_tx: std_mpsc::Sender<Payload>,
) -> impl Future<Item = (), Error = io::Error> + Send + 'static
where
    T: AsyncRead + AsyncWrite,
    T: Send + Sync,
    T: 'static,
{
    let keys = stream::iter_ok::<_, ()>(keys);
    client.create_channel().and_then(move |channel| {
        let id = channel.id;
        debug!("created channel with id: {}", id);

        let ch1 = channel.clone();
        let ch2 = channel.clone();

        channel
            .exchange_declare(
                EXCHANGE,
                EXCHANGE_TYPE,
                ExchangeDeclareOptions::default(),
                FieldTable::new(),
            )
            .and_then(move |_| {
                channel
                    .queue_declare(name, QueueDeclareOptions::default(), FieldTable::new())
                    .and_then(move |queue| {
                        debug!("channel {} declared queue {}", id, name);

                        keys.for_each(move |key| {
                            channel
                                .queue_bind(
                                    name,
                                    EXCHANGE,
                                    &key,
                                    QueueBindOptions::default(),
                                    FieldTable::new(),
                                )
                                .map(|_| ())
                                .map_err(|_| ())
                        }).map_err(|_| Error::new(ErrorKind::Other, "queue_bind error!"))
                            .map(|_| queue)
                    })
                    .and_then(move |queue| {
                        ch1.basic_consume(
                            &queue,
                            name,
                            BasicConsumeOptions::default(),
                            FieldTable::new(),
                        )
                    })
            })
            .and_then(|stream| {
                stream.for_each(move |message| {
                    let Delivery {
                        delivery_tag,
                        routing_key,
                        data,
                        ..
                    } = message;
                    let ret = consumer_tx.send((routing_key, data));
                    if ret.is_err() {
                        error!("amqp message send error {:?}", ret);
                    }
                    ch2.basic_ack(delivery_tag, false)
                })
            })
    })
}

pub fn start_amqp(
    addr: SocketAddr,
    name: &'static str,
    keys: Vec<String>,
) -> (mpsc::Sender<Payload>, std_mpsc::Receiver<Payload>) {
    let (publisher_tx, publisher_rx) = mpsc::channel(65535);
    let (consumer_tx, consumer_rx) = std_mpsc::channel();

    thread::spawn(move || {
        Runtime::new()
            .unwrap()
            .block_on_all(connect_publisher(addr, name, publisher_rx))
    });

    thread::spawn(move || {
        Runtime::new()
            .unwrap()
            .block_on_all(connect_consumer(addr, name, keys, consumer_tx))
    });

    (publisher_tx, consumer_rx)
}

#[cfg(test)]
mod test {
    use super::*;
    #[test]
    // #[ignore]
    fn basics() {
        let addr = "127.0.0.1:5672".parse().unwrap();

        let (mut tx, rx) = start_amqp(
            addr,
            "network",
            vec!["chain.newtx".to_string(), "chain.newblk".to_string()],
        );

        tx.try_send(("chain.newtx".to_string(), vec![123])).unwrap();

        assert_eq!(rx.recv().unwrap(), ("chain.newtx".to_string(), vec![123]));
    }
}
