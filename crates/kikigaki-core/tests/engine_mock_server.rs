use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use kikigaki_core::config::RemoteConfig;
use kikigaki_core::engine::{Engine, EngineCmd, EngineMsg};
use kikigaki_core::remote::{RemoteEngine, SidecarProcess, SidecarSpawner};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

struct NoSidecar;

impl SidecarSpawner for NoSidecar {
    fn spawn(&self, _cfg: &RemoteConfig) -> anyhow::Result<Option<Box<dyn SidecarProcess>>> {
        Ok(None)
    }
}

fn remote_config(addr: std::net::SocketAddr) -> RemoteConfig {
    RemoteConfig {
        ws_url: format!("ws://{addr}/ingest"),
        spawn_sidecar: false,
        connect_timeout_ms: 1_000,
        ..RemoteConfig::default()
    }
}

fn recv(engine: &mut RemoteEngine) -> EngineMsg {
    engine
        .events()
        .recv_timeout(Duration::from_secs(2))
        .expect("remote engine event")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ready_end_final_disconnect_and_bounded_shutdown_with_sink_clone() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        assert_eq!(
            socket.next().await.unwrap().unwrap(),
            Message::Text(r#"{"sr":16000,"format":"pcm_s16le","channels":1}"#.to_owned().into())
        );
        socket
            .send(Message::Text(r#"{"type":"ready","sr":16000}"#.into()))
            .await
            .unwrap();
        assert!(matches!(
            socket.next().await.unwrap().unwrap(),
            Message::Binary(bytes) if bytes.len() == 3_200
        ));
        assert!(matches!(
            socket.next().await.unwrap().unwrap(),
            Message::Binary(bytes) if bytes.len() == 16_000
        ));
        socket
            .send(Message::Text(
                r#"{"type":"final","text":"こんにちは。","lang":"ja","latency_ms":80.0}"#.into(),
            ))
            .await
            .unwrap();
        socket.close(None).await.unwrap();
    });

    let mut engine = RemoteEngine::start(remote_config(addr), Box::new(NoSidecar)).unwrap();
    assert!(matches!(recv(&mut engine), EngineMsg::Ready));
    let sink = engine.sink();
    let surviving_clone = sink.clone();
    sink.send(EngineCmd::Begin { gen: 41 }).unwrap();
    sink.send(EngineCmd::Audio(vec![0.0; 1_600])).unwrap();
    sink.send(EngineCmd::End {
        gen: 41,
        pad_ms: 500,
    })
    .unwrap();
    assert!(matches!(
        recv(&mut engine),
        EngineMsg::Final {
            gen: 41,
            text,
            engine_latency_ms: Some(80.0),
            ..
        } if text == "こんにちは。"
    ));
    assert!(matches!(recv(&mut engine), EngineMsg::Disconnected { .. }));
    server.await.unwrap();

    let started = Instant::now();
    Box::new(engine).shutdown();
    assert!(started.elapsed() < Duration::from_secs(2));
    drop(surviving_clone);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finals_before_and_after_end_keep_the_begin_generation() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        socket.next().await.unwrap().unwrap();
        socket
            .send(Message::Text(r#"{"type":"ready"}"#.into()))
            .await
            .unwrap();
        socket.next().await.unwrap().unwrap();
        socket
            .send(Message::Text(
                r#"{"type":"final","text":"before","lang":"ja"}"#.into(),
            ))
            .await
            .unwrap();
        socket.next().await.unwrap().unwrap();
        socket
            .send(Message::Text(
                r#"{"type":"final","text":"after","lang":"ja"}"#.into(),
            ))
            .await
            .unwrap();
    });

    let mut engine = RemoteEngine::start(remote_config(addr), Box::new(NoSidecar)).unwrap();
    assert!(matches!(recv(&mut engine), EngineMsg::Ready));
    let sink = engine.sink();
    sink.send(EngineCmd::Begin { gen: 7 }).unwrap();
    sink.send(EngineCmd::Audio(vec![0.0; 320])).unwrap();
    assert!(matches!(recv(&mut engine), EngineMsg::Final { gen: 7, text, .. } if text == "before"));
    sink.send(EngineCmd::End {
        gen: 7,
        pad_ms: 500,
    })
    .unwrap();
    assert!(matches!(recv(&mut engine), EngineMsg::Final { gen: 7, text, .. } if text == "after"));
    server.await.unwrap();
    Box::new(engine).shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn final_after_second_begin_uses_the_new_generation() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        socket.next().await.unwrap().unwrap();
        socket
            .send(Message::Text(r#"{"type":"ready"}"#.into()))
            .await
            .unwrap();
        socket.next().await.unwrap().unwrap();
        socket
            .send(Message::Text(
                r#"{"type":"final","text":"first","lang":"ja"}"#.into(),
            ))
            .await
            .unwrap();
        socket.next().await.unwrap().unwrap();
        socket
            .send(Message::Text(
                r#"{"type":"final","text":"second","lang":"ja"}"#.into(),
            ))
            .await
            .unwrap();
    });

    let mut engine = RemoteEngine::start(remote_config(addr), Box::new(NoSidecar)).unwrap();
    assert!(matches!(recv(&mut engine), EngineMsg::Ready));
    let sink = engine.sink();
    sink.send(EngineCmd::Begin { gen: 1 }).unwrap();
    sink.send(EngineCmd::Audio(vec![0.0; 320])).unwrap();
    assert!(matches!(recv(&mut engine), EngineMsg::Final { gen: 1, text, .. } if text == "first"));
    sink.send(EngineCmd::Begin { gen: 2 }).unwrap();
    sink.send(EngineCmd::Audio(vec![0.0; 320])).unwrap();
    assert!(matches!(recv(&mut engine), EngineMsg::Final { gen: 2, text, .. } if text == "second"));
    server.await.unwrap();
    Box::new(engine).shutdown();
}
