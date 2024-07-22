use futures::stream::StreamExt;
use libp2p::{gossipsub, mdns, noise, swarm::NetworkBehaviour, swarm::SwarmEvent, tcp, yamux};
use std::collections::hash_map::DefaultHasher;
use std::error::Error;
use std::hash::{Hash, Hasher};
use std::thread;
use std::time::Duration;
use tokio::runtime::Runtime;
use tokio::{io, io::AsyncBufReadExt, select};
use tracing_subscriber::EnvFilter;

#[derive(NetworkBehaviour)]
struct MyBehaviour {
    gossipsub: gossipsub::Behaviour,
    mdns: mdns::tokio::Behaviour,
}

fn setup_tracing() -> Result<(), Box<dyn Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .try_init()
        .map_err(|e| Box::<dyn Error + Send + Sync>::from(e))
}

fn build_swarm() -> Result<libp2p::Swarm<MyBehaviour>, Box<dyn Error + Send + Sync>> {
    let swarm = libp2p::SwarmBuilder::with_new_identity()
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_quic()
        .with_behaviour(|key| {
            let message_id_fn = |message: &gossipsub::Message| {
                let mut s = DefaultHasher::new();
                message.data.hash(&mut s);
                gossipsub::MessageId::from(s.finish().to_string())
            };

            let gossipsub_config = gossipsub::ConfigBuilder::default()
                .heartbeat_interval(Duration::from_secs(10))
                .validation_mode(gossipsub::ValidationMode::Strict)
                .message_id_fn(message_id_fn)
                .build()
                .map_err(|msg| io::Error::new(io::ErrorKind::Other, msg))?;

            let gossipsub = gossipsub::Behaviour::new(
                gossipsub::MessageAuthenticity::Signed(key.clone()),
                gossipsub_config,
            )?;

            let mdns =
                mdns::tokio::Behaviour::new(mdns::Config::default(), key.public().to_peer_id())?;
            Ok(MyBehaviour { gossipsub, mdns })
        })?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
        .build();

    Ok(swarm)
}

async fn run_node() -> Result<(), Box<dyn Error + Send + Sync>> {
    setup_tracing()?;

    let mut swarm = build_swarm()?;

    let topic = gossipsub::IdentTopic::new("test-net");
    swarm.behaviour_mut().gossipsub.subscribe(&topic)?;

    let mut stdin = io::BufReader::new(io::stdin()).lines();

    swarm.listen_on("/ip4/0.0.0.0/udp/0/quic-v1".parse()?)?;
    swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()?)?;

    println!("Enter messages via STDIN and they will be sent to connected peers using Gossipsub");

    loop {
        select! {
            Ok(Some(line)) = stdin.next_line() => {
                send_message(&mut swarm, topic.clone(), line.as_bytes()).await?;
            }
            event = swarm.select_next_some() => {
                handle_event(&mut swarm, event).await?;
            }
        }
    }
}

async fn send_message(
    swarm: &mut libp2p::Swarm<MyBehaviour>,
    topic: gossipsub::IdentTopic,
    message: &[u8],
) -> Result<(), Box<dyn Error + Send + Sync>> {
    if let Err(e) = swarm.behaviour_mut().gossipsub.publish(topic, message) {
        println!("Publish error: {e:?}");
    }
    Ok(())
}

async fn handle_event(
    swarm: &mut libp2p::Swarm<MyBehaviour>,
    event: SwarmEvent<MyBehaviourEvent>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    match event {
        SwarmEvent::Behaviour(MyBehaviourEvent::Mdns(mdns::Event::Discovered(list))) => {
            for (peer_id, _multiaddr) in list {
                println!("mDNS discovered a new peer: {peer_id}");
                swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer_id);
            }
        }
        SwarmEvent::Behaviour(MyBehaviourEvent::Mdns(mdns::Event::Expired(list))) => {
            for (peer_id, _multiaddr) in list {
                println!("mDNS discovered peer has expired: {peer_id}");
                swarm
                    .behaviour_mut()
                    .gossipsub
                    .remove_explicit_peer(&peer_id);
            }
        }
        SwarmEvent::Behaviour(MyBehaviourEvent::Gossipsub(gossipsub::Event::Message {
            propagation_source: peer_id,
            message_id: id,
            message,
        })) => println!(
            "Got message: '{}' with id: {id} from peer: {peer_id}",
            String::from_utf8_lossy(&message.data),
        ),
        SwarmEvent::NewListenAddr { address, .. } => {
            println!("Local node is listening on {address}");
        }
        _ => {}
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut handles = vec![];

    for _ in 0..3 {
        let handle = thread::spawn(|| {
            let rt = Runtime::new().unwrap();
            rt.block_on(async {
                if let Err(e) = run_node().await {
                    eprintln!("Error: {:?}", e);
                }
            });
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }

    Ok(())
}
