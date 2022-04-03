use futures::StreamExt;
use log::info;
use ratsio::{NatsClient, NatsClientOptions, RatsioError};
use std::env;

pub fn logger_setup() {
    use env_logger::Builder;
    use log::LevelFilter;
    use std::io::Write;

    let _ = Builder::new()
        .format(|buf, record| writeln!(buf, "[{}] - {}", record.level(), record.args()))
        .filter(None, LevelFilter::Trace)
        .try_init();
}

#[tokio::main]
async fn main() -> Result<(), RatsioError> {
    logger_setup();
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        eprintln!("Usage: {} <subject>", args[0]);
        return Err(RatsioError::GenericError("Invalid input".into()));
    }

    let subject = args[1].clone();
    //Create nats client
    let mut options = NatsClientOptions::default();
    options.username = "user".into();
    options.password = "secret".into();
    options.cluster_uris = vec!["nats://localhost:4222".to_string()].into();
    let nats_client = NatsClient::new(options).await?;

    //subscribe to nats subject 'foo'
    let (sid, mut subscription) = nats_client.subscribe(subject.clone()).await?;

    ctrlc::set_handler(move || {
        let mut runtime = tokio::runtime::Runtime::new().unwrap();
        let _ = runtime.block_on(nats_client.un_subscribe(&sid));
    })
    .expect("Error setting Ctrl-C handler");

    //Listen for messages on the 'foo' description
    //The loop terminates when the upon un_subscribe
    while let Some(message) = subscription.next().await {
        info!(
            "{:?}\n\t{:?}",
            &message,
            String::from_utf8_lossy(message.payload.as_ref())
        );
    }
    Ok(())
}
