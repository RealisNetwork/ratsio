use crate::error::RatsioError;
use crate::nats_client::{ClosableMessage, NatsClient};
use crate::nuid::NUID;
use crate::protocol;
use crate::stan_client::{
    AckHandler, ClientInfo, StanClient, StanMessage, StanOptions, StanSid, StanSubscribe,
    StartPosition, Subscription, DEFAULT_ACK_WAIT, DEFAULT_DISCOVER_PREFIX, DEFAULT_MAX_INFLIGHT,
};
use futures::{Stream, StreamExt};
use nom::lib::std::collections::HashMap;
use pin_project::pin_project;
use prost::Message;
use sha2::{Digest, Sha256};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::RwLock;

impl From<protocol::ConnectResponse> for ClientInfo {
    fn from(protocol: protocol::ConnectResponse) -> ClientInfo {
        ClientInfo {
            pub_prefix: protocol.pub_prefix.clone(),
            sub_requests: protocol.sub_requests.clone(),
            unsub_requests: protocol.unsub_requests.clone(),
            close_requests: protocol.close_requests,
        }
    }
}

impl StanClient {
    pub async fn from_options(options: StanOptions) -> Result<Arc<Self>, RatsioError> {
        let id_generator = Arc::new(RwLock::new({
            let mut id_gen = NUID::new();
            id_gen.randomize_prefix();
            id_gen
        }));

        let conn_id = id_generator.write().await.next();
        debug!("Connection id => {}", &conn_id);
        let heartbeat_inbox: String = format!("_HB.{}", id_generator.write().await.next());
        let discover_subject: String =
            format!("{}.{}", DEFAULT_DISCOVER_PREFIX, options.cluster_id);
        let client_id = options.client_id.clone();

        let mut nats_options = options.nats_options.clone();
        nats_options.name = client_id.clone();
        nats_options.subscribe_on_reconnect = true;

        let nats_client = NatsClient::new(options.nats_options.clone()).await?;
        debug!("Connecting STAN Client");
        let connect_request = protocol::ConnectRequest {
            client_id: client_id.clone(),
            conn_id: conn_id.clone().as_bytes().into(),
            heartbeat_inbox: heartbeat_inbox.clone(),
            ..Default::default()
        };

        let mut connect_request_buf: Vec<u8> = Vec::with_capacity(64);
        connect_request.encode(&mut connect_request_buf).unwrap();
        let connect_response = nats_client
            .request(discover_subject, connect_request_buf.as_slice())
            .await?;
        let connect_response =
            protocol::ConnectResponse::decode(connect_response.payload.as_slice())?;
        let client_info: ClientInfo = connect_response.clone().into();

        let stan_client = Arc::new(StanClient {
            //subs_tx: Arc::new(RwLock::new(HashMap::default())),
            options: StanOptions {
                nats_options,
                ..options
            },
            nats_client: nats_client.clone(),
            client_id: client_id.clone(),
            conn_id: RwLock::new(conn_id.clone().into_bytes()),
            client_info: Arc::new(RwLock::new(client_info)),
            id_generator: id_generator.clone(),
            subscriptions: RwLock::new(HashMap::default()),
            self_reference: RwLock::new(None),
        });
        *stan_client.self_reference.write().await = Some(stan_client.clone());

        tokio::spawn(async move {
            let _ = StanClient::process_heartbeats(
                nats_client.clone(),
                id_generator.clone(),
                conn_id.clone().into_bytes(),
                client_id.clone(),
                heartbeat_inbox.clone(),
            )
            .await;
        });

        let reconnect_stan_client = stan_client.clone();
        stan_client
            .nats_client
            .add_disconnect_handler(Box::new(move |_nats_client| {
                let stan_client = reconnect_stan_client.clone();
                tokio::spawn(async move {
                    let _ = stan_client.clone().on_reconnect().await;
                });
            }))
            .await?;

        Ok(stan_client)
    }

    async fn process_heartbeats(
        nats_client: Arc<NatsClient>,
        id_generator: Arc<RwLock<NUID>>,
        conn_id: Vec<u8>,
        client_id: String,
        heartbeat_inbox: String,
    ) -> Result<(), RatsioError> {
        debug!("Subscribing to heartbeat => {}", &heartbeat_inbox);
        let (_sid, mut heartbeats) = nats_client.subscribe(heartbeat_inbox.clone()).await?;
        while let Some(msg) = heartbeats.next().await {
            if let Some(reply_to) = msg.reply_to {
                let reply_msg = protocol::PubMsg {
                    client_id: client_id.clone(),
                    conn_id: conn_id.clone(),
                    subject: heartbeat_inbox.clone(),
                    guid: id_generator.write().await.next(),
                    ..Default::default()
                };

                let mut req_buf: Vec<u8> = Vec::with_capacity(64);
                reply_msg.encode(&mut req_buf).unwrap();
                match nats_client.publish(reply_to, req_buf.as_slice()).await {
                    Ok(_) => {
                        //info!("HEARTBEAT -- heartbeat reply was sent");
                    }
                    Err(err) => {
                        error!("Error replying to heartbeat {:?}", err);
                    }
                }
            }
        }
        Ok(())
    }

    async fn on_reconnect(&self) -> Result<(), RatsioError> {
        Ok(())
    }

    // Subscribe will perform a subscription with the given options to the cluster.
    //
    // If no option is specified, DefaultSubscriptionOptions are used. The default start
    // position is to receive new messages only (messages published after the subscription is
    // registered in the cluster).
    pub async fn subscribe<T>(
        &self,
        subject: T,
        queue_group: Option<T>,
        durable_name: Option<T>,
    ) -> Result<(StanSid, impl Stream<Item = StanMessage> + Send + Sync), RatsioError>
    where
        T: ToString,
    {
        self.subscribe_inner(
            subject.to_string(),
            queue_group.map(|i| i.to_string()),
            durable_name.map(|i| i.to_string()),
            DEFAULT_MAX_INFLIGHT,
            DEFAULT_ACK_WAIT,
            StartPosition::LastReceived,
            0,
            None,
            false,
        )
        .await
    }

    pub async fn subscribe_with_manual_ack<T>(
        &self,
        subject: T,
        queue_group: Option<T>,
        durable_name: Option<T>,
    ) -> Result<(StanSid, impl Stream<Item = StanMessage> + Send + Sync), RatsioError>
    where
        T: ToString,
    {
        self.subscribe_inner(
            subject.to_string(),
            queue_group.map(|i| i.to_string()),
            durable_name.map(|i| i.to_string()),
            DEFAULT_MAX_INFLIGHT,
            DEFAULT_ACK_WAIT,
            StartPosition::LastReceived,
            0,
            None,
            true,
        )
        .await
    }

    pub async fn subscribe_with_all<T>(
        &self,
        subject: T,
        queue_group: Option<T>,
        durable_name: Option<T>,
        max_in_flight: i32,
        ack_wait_in_secs: i32,
        start_position: StartPosition,
        start_sequence: u64,
        start_time_delta: Option<i32>,
        manual_acks: bool,
    ) -> Result<(StanSid, impl Stream<Item = StanMessage> + Send + Sync), RatsioError>
    where
        T: ToString,
    {
        self.subscribe_inner(
            subject.to_string(),
            queue_group.map(|i| i.to_string()),
            durable_name.map(|i| i.to_string()),
            max_in_flight,
            ack_wait_in_secs,
            start_position,
            start_sequence,
            start_time_delta,
            manual_acks,
        )
        .await
    }

    pub async fn subscribe_with<T>(
        &self,
        stan_subscribe: T,
    ) -> Result<(StanSid, impl Stream<Item = StanMessage> + Send + Sync), RatsioError>
    where
        T: Into<StanSubscribe>,
    {
        let stan_subscribe = stan_subscribe.into();
        self.subscribe_inner(
            stan_subscribe.subject.clone(),
            stan_subscribe.queue_group.clone(),
            stan_subscribe.durable_name.clone(),
            stan_subscribe.max_in_flight,
            stan_subscribe.ack_wait_in_secs,
            stan_subscribe.start_position.clone(),
            stan_subscribe.start_sequence,
            stan_subscribe.start_time_delta,
            stan_subscribe.manual_acks,
        )
        .await
    }

    async fn subscribe_inner(
        &self,
        subject: String,
        queue_group: Option<String>,
        durable_name: Option<String>,
        max_in_flight: i32,
        ack_wait_in_secs: i32,
        start_position: StartPosition,
        start_sequence: u64,
        start_time_delta: Option<i32>,
        manual_acks: bool,
    ) -> Result<(StanSid, impl Stream<Item = StanMessage> + Send + Sync), RatsioError> {
        let client_info = self.client_info.read().await.clone();
        let inbox: String = format!("_SUB.{}", self.id_generator.write().await.next());

        let sub_request = protocol::SubscriptionRequest {
            client_id: self.client_id.clone(),
            subject: subject.to_string(),
            q_group: queue_group.clone().unwrap_or_default(),
            durable_name: durable_name.clone().unwrap_or_default(),
            ack_wait_in_secs,
            max_in_flight,
            start_sequence,
            start_time_delta: (start_time_delta.unwrap_or_default() as i64),
            start_position: match start_position {
                StartPosition::NewOnly => protocol::StartPosition::NewOnly as i32, // Отправляет только новые сообщения
                StartPosition::LastReceived => protocol::StartPosition::LastReceived as i32, // Отправляет только последнее полученное сообщение
                StartPosition::TimeDeltaStart => protocol::StartPosition::TimeDeltaStart as i32, // Отправляет сообщения из определенного промежутка
                StartPosition::SequenceStart => protocol::StartPosition::SequenceStart as i32, // Отправляет сообщения начинающиеся с последовательности
                StartPosition::First => protocol::StartPosition::First as i32, // Все
            },
            inbox: inbox.clone(),
        };

        let mut su_req_buf: Vec<u8> = Vec::with_capacity(64);
        sub_request.encode(&mut su_req_buf).unwrap();

        if let Ok(sub_response) = self
            .nats_client
            .request(&client_info.sub_requests, su_req_buf.as_slice())
            .await
        {
            let sub_response =
                protocol::SubscriptionResponse::decode(&sub_response.payload[..]).unwrap();
            let ack_inbox = sub_response.ack_inbox.clone();
            let (sid, mut subscription) = self.nats_client.subscribe(inbox.clone()).await?;

            let mut subscriptions = self.subscriptions.write().await;
            let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
            let stan_sid = StanSid(sid);
            let sub = Subscription {
                subject: subject.clone(),
                durable_name: durable_name.clone(),
                inbox: inbox.clone(),
                sender: sender.clone(),
            };
            subscriptions.insert((stan_sid.0).0.clone(), sub);

            tokio::spawn(async move {
                while let Some(nats_msg) = subscription.next().await {
                    let _ = sender.send(ClosableMessage::Message(nats_msg));
                }
            });

            Ok((
                stan_sid,
                StanClosableReceiver {
                    receiver,
                    ack_inbox,
                    manual_acks,
                    stan_client: self.get_self_reference().await,
                },
            ))
        } else {
            Err(RatsioError::InternalServerError)
        }
    }

    async fn get_self_reference(&self) -> Arc<StanClient> {
        self.self_reference.read().await.clone().unwrap()
    }

    async fn ack_message(
        &self,
        ack_inbox: String,
        subject: String,
        sequence: u64,
    ) -> Result<(), RatsioError> {
        let ack_request = protocol::Ack { subject, sequence };
        let mut ack_req_buf: Vec<u8> = Vec::with_capacity(64);
        ack_request.encode(&mut ack_req_buf).unwrap();
        self.nats_client.publish(ack_inbox, &ack_req_buf[..]).await
    }

    pub async fn acknowledge(&self, message: StanMessage) -> Result<(), RatsioError> {
        match message.ack_inbox.clone() {
            Some(ack_inbox) => {
                self.ack_message(ack_inbox, message.subject.clone(), message.sequence)
                    .await
            }
            None => Err(RatsioError::AckInboxMissing),
        }
    }

    pub async fn publish<T>(&self, subject: T, payload: &[u8]) -> Result<(), RatsioError>
    where
        T: ToString,
    {
        self.send_inner(subject.to_string(), None, payload).await
    }

    pub async fn send_with_reply<T>(
        &self,
        subject: T,
        reply_to: T,
        payload: &[u8],
    ) -> Result<(), RatsioError>
    where
        T: ToString,
    {
        self.send_inner(subject.to_string(), Some(reply_to.to_string()), payload)
            .await
    }

    pub async fn send_with<T>(&self, message: T) -> Result<(), RatsioError>
    where
        T: Into<StanMessage>,
    {
        let message = message.into();
        self.send_inner(
            message.subject.clone(),
            message.reply_to.clone(),
            &message.payload[..],
        )
        .await
    }

    async fn send_inner(
        &self,
        subject: String,
        reply_to: Option<String>,
        payload: &[u8],
    ) -> Result<(), RatsioError> {
        let mut hasher = Sha256::new();
        hasher.update(payload);

        let conn_id = self.conn_id.read().await.clone();
        let guid = self.id_generator.write().await.next();
        let pub_msg = protocol::PubMsg {
            sha256: Vec::from(&hasher.finalize()[..]),
            client_id: self.client_id.clone(),
            subject: subject.clone(),
            reply: reply_to.unwrap_or_default(),
            data: payload.into(),
            guid: guid.clone(),
            conn_id,
        };

        let mut pub_req_buf: Vec<u8> = Vec::with_capacity(64);
        pub_msg.encode(&mut pub_req_buf).unwrap();

        let client_info = self.client_info.read().await;
        let subject = format!("{}.{}", client_info.pub_prefix, subject);
        self.nats_client
            .publish(subject, pub_req_buf.as_slice())
            .await
    }

    pub async fn un_subscribe(&self, stan_sid: &StanSid) -> Result<(), RatsioError> {
        let client_info = self.client_info.read().await;
        let mut subscriptions = self.subscriptions.write().await;

        if let Some(subscription) = subscriptions.remove(&(stan_sid.0).0) {
            let unsub_msg = protocol::UnsubscribeRequest {
                client_id: self.client_id.clone(),
                subject: subscription.subject.clone(),
                inbox: subscription.inbox.clone(),
                durable_name: subscription.durable_name.clone().unwrap_or_default(),
            };

            let mut unsub_req_buf: Vec<u8> = Vec::with_capacity(64);
            unsub_msg.encode(&mut unsub_req_buf).unwrap();

            let _ = self
                .nats_client
                .publish(client_info.unsub_requests.clone(), unsub_req_buf.as_slice())
                .await;
            let _ = subscription.sender.send(ClosableMessage::Close);
            self.nats_client.un_subscribe(&stan_sid.0).await
        } else {
            Ok(())
        }
    }

    pub async fn close(&self) -> Result<(), RatsioError> {
        let client_info = self.client_info.read().await;
        let nats_client = self.nats_client.clone();
        let client_id = self.client_id.clone();
        let close_request = protocol::CloseRequest { client_id };
        let mut close_req_buf: Vec<u8> = Vec::with_capacity(64);
        close_request.encode(&mut close_req_buf).unwrap();
        let _ = nats_client
            .publish(client_info.close_requests.clone(), &close_req_buf[..])
            .await?;
        *self.self_reference.write().await = None;
        self.nats_client.close().await
    }
}

#[pin_project]
struct StanClosableReceiver {
    #[pin]
    receiver: UnboundedReceiver<ClosableMessage>,
    ack_inbox: String,
    manual_acks: bool,
    stan_client: Arc<StanClient>,
}

impl Stream for StanClosableReceiver {
    type Item = StanMessage;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();
        let ack_inbox = this.ack_inbox.clone();
        let manual_acks = this.manual_acks;
        let stan_client = this.stan_client.clone();
        match this.receiver.poll_recv(cx) {
            Poll::Ready(Some(ClosableMessage::Message(nats_msg))) => {
                let msg = protocol::MsgProto::decode(&nats_msg.payload[..]).unwrap();
                let subject = msg.subject.clone();
                let sequence = msg.sequence;
                let ack_ack_inbox = this.ack_inbox.clone();
                let ack_subject = subject;
                let ack_handler = if !*manual_acks {
                    Some(AckHandler(Box::new(move || {
                        let ack_inbox2 = ack_ack_inbox.clone();
                        let subject2 = ack_subject.clone();
                        let stan_client2 = stan_client.clone();
                        tokio::spawn(async move {
                            //debug!("stan ack - message <=> {} ", &subject2);
                            let _ = stan_client2
                                .ack_message(ack_inbox2, subject2, sequence)
                                .await;
                        });
                    })))
                } else {
                    None
                };
                let stan_msg = StanMessage {
                    subject: msg.subject,
                    reply_to: if !msg.reply.is_empty() {
                        Some(msg.reply)
                    } else {
                        None
                    },
                    payload: msg.data,
                    timestamp: msg.timestamp,
                    sequence: msg.sequence,
                    redelivered: msg.redelivered,
                    ack_inbox: Some(ack_inbox),
                    ack_handler,
                };
                Poll::Ready(Some(stan_msg))
            }
            Poll::Ready(Some(ClosableMessage::Close)) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(None),
        }
    }
}
