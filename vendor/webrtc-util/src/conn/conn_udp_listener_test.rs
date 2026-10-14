use std::future::Future;
use std::pin::Pin;

use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::Duration;

use super::conn_udp_listener::*;
use super::*;
use crate::error::{Error, Result};

async fn pipe() -> Result<(
    Arc<dyn Listener + Send + Sync>,
    Arc<dyn Conn + Send + Sync>,
    UdpSocket,
)> {
    // Start listening
    let listener = Arc::new(listen("0.0.0.0:0").await?);

    // Open a connection
    let d_conn = UdpSocket::bind("0.0.0.0:0").await?;
    d_conn.connect(listener.addr().await?).await?;

    // Write to the connection to initiate it
    let handshake = "hello";
    d_conn.send(handshake.as_bytes()).await?;
    let daddr = d_conn.local_addr()?;

    // Accept the connection
    let (l_conn, raddr) = listener.accept().await?;
    assert_eq!(daddr, raddr, "remote address should be match");

    let raddr = l_conn.remote_addr();
    if let Some(raddr) = raddr {
        assert_eq!(daddr, raddr, "remote address should be match");
    } else {
        panic!("expected Some, but got None, for remote_addr()");
    }

    let mut buf = vec![0u8; handshake.len()];
    let n = l_conn.recv(&mut buf).await?;

    let result = String::from_utf8(buf[..n].to_vec())?;
    if handshake != result {
        Err(Error::Other(format!(
            "errHandshakeFailed: {handshake} != {result}"
        )))
    } else {
        Ok((listener, l_conn, d_conn))
    }
}

#[tokio::test]
async fn test_listener_close_timeout() -> Result<()> {
    let (listener, ca, _) = pipe().await?;

    listener.close().await?;

    // Close client after server closes to cleanup
    ca.close().await?;

    Ok(())
}

#[tokio::test]
async fn test_listener_close_unaccepted() -> Result<()> {
    const BACKLOG: usize = 2;

    let listener = ListenConfig {
        backlog: BACKLOG,
        ..Default::default()
    }
    .listen("0.0.0.0:0")
    .await?;

    for i in 0..BACKLOG as u8 {
        let conn = UdpSocket::bind("0.0.0.0:0").await?;
        conn.connect(listener.addr().await?).await?;
        conn.send(&[i]).await?;
        conn.close().await?;
    }

    // Wait all packets being processed by readLoop
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Unaccepted connections must be closed by listener.Close()
    listener.close().await?;

    Ok(())
}

#[tokio::test]
async fn test_listener_accept_filter() -> Result<()> {
    let tests = vec![("CreateConn", &[0xAA], true), ("Discarded", &[0x00], false)];

    for (name, packet, expected) in tests {
        let accept_filter: Option<AcceptFilterFn> = Some(Box::new(
            |pkt: &[u8]| -> Pin<Box<dyn Future<Output = bool> + Send + 'static>> {
                let p0 = pkt[0];
                Box::pin(async move { p0 == 0xAA })
            },
        ));

        let listener = Arc::new(
            ListenConfig {
                accept_filter,
                ..Default::default()
            }
            .listen("0.0.0.0:0")
            .await?,
        );

        let conn = UdpSocket::bind("0.0.0.0:0").await?;
        conn.connect(listener.addr().await?).await?;
        conn.send(packet).await?;

        let (ch_accepted_tx, mut ch_accepted_rx) = mpsc::channel::<()>(1);
        let mut ch_accepted_tx = Some(ch_accepted_tx);
        let listener2 = Arc::clone(&listener);
        tokio::spawn(async move {
            let (c, _raddr) = match listener2.accept().await {
                Ok((c, raddr)) => (c, raddr),
                Err(err) => {
                    assert_eq!(Error::ErrClosedListener, err);
                    return Result::<()>::Ok(());
                }
            };

            ch_accepted_tx.take();
            c.close().await?;

            Result::<()>::Ok(())
        });

        let mut accepted = false;
        let mut timeout = false;
        let timer = tokio::time::sleep(Duration::from_millis(10));
        tokio::pin!(timer);
        tokio::select! {
            _= ch_accepted_rx.recv()=>{
                accepted = true;
            }
            _ = timer.as_mut() => {
                timeout = true;
            }
        }

        assert_eq!(accepted, expected, "{name}: unexpected result");
        assert_eq!(!timeout, expected, "{name}: unexpected result");

        conn.close().await?;
        listener.close().await?;
    }
    Ok(())
}

#[tokio::test]
async fn test_listener_concurrent() -> Result<()> {
    const BACKLOG: usize = 2;

    let listener = Arc::new(
        ListenConfig {
            backlog: BACKLOG,
            ..Default::default()
        }
        .listen("0.0.0.0:0")
        .await?,
    );

    for i in 0..BACKLOG as u8 + 1 {
        let conn = UdpSocket::bind("0.0.0.0:0").await?;
        conn.connect(listener.addr().await?).await?;
        conn.send(&[i]).await?;
        conn.close().await?;
    }

    // Wait all packets being processed by readLoop
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut b = vec![0u8; 1];
    for i in 0..BACKLOG as u8 {
        let (conn, _raddr) = listener.accept().await?;
        let n = conn.recv(&mut b).await?;
        assert_eq!(
            &b[..n],
            &[i],
            "Packet from connection {} is wrong, expected: [{}], got: {:?}",
            i,
            i,
            &b[..n]
        );
        conn.close().await?;
    }

    let (done_tx, mut done_rx) = mpsc::channel::<()>(1);
    let mut done_tx = Some(done_tx);
    let listener2 = Arc::clone(&listener);
    tokio::spawn(async move {
        match listener2.accept().await {
            Ok((conn, _raddr)) => {
                conn.close().await?;
            }
            Err(err) => {
                assert!(Error::ErrClosedListener == err || Error::ErrClosedListenerAcceptCh == err);
            }
        }

        done_tx.take();

        Result::<()>::Ok(())
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    listener.close().await?;

    let _ = done_rx.recv().await;

    Ok(())
}

/// a socket on loopback that sent `packet` to `listener`
async fn remote(listener: &impl Listener, packet: &[u8]) -> Result<UdpSocket> {
    let remote = UdpSocket::bind("127.0.0.1:0").await?;
    remote.connect(listener.addr().await?).await?;
    remote.send(packet).await?;
    Ok(remote)
}

/// whether reading `conn` fails because it is closed, instead of blocking
async fn is_closed(conn: &Arc<dyn Conn + Send + Sync>) -> bool {
    let mut b = [0u8; 8];
    let read = tokio::time::timeout(Duration::from_secs(1), conn.recv(&mut b)).await;
    matches!(read, Ok(Err(Error::ErrBufferClosed)))
}

/// the next accepted conn, unless none arrives within 200 ms
async fn accepted(listener: &impl Listener) -> Option<(Arc<dyn Conn + Send + Sync>, SocketAddr)> {
    tokio::time::timeout(Duration::from_millis(200), listener.accept())
        .await
        .ok()
        .map(|c| c.expect("accept"))
}

#[tokio::test]
async fn test_listener_close_forgets_conn() -> Result<()> {
    let listener = listen("127.0.0.1:0").await?;
    let remote = remote(&listener, &[1]).await?;
    let (conn, raddr) = accepted(&listener).await.expect("first conn");
    let mut b = [0u8; 8];
    assert_eq!(conn.recv(&mut b).await?, 1);

    conn.close().await?;
    assert!(is_closed(&conn).await);

    // the remote is forgotten: its next packet makes a new conn
    remote.send(&[2]).await?;
    let (again, raddr_again) = accepted(&listener).await.expect("new conn");
    assert_eq!(raddr, raddr_again);
    assert!(!Arc::ptr_eq(&conn, &again));
    let n = again.recv(&mut b).await?;
    assert_eq!(&b[..n], &[2]);

    // closing the old conn again leaves the new one in place
    conn.close().await?;
    remote.send(&[3]).await?;
    let n = again.recv(&mut b).await?;
    assert_eq!(&b[..n], &[3]);
    assert!(accepted(&listener).await.is_none());

    listener.close().await?;
    Ok(())
}

#[tokio::test]
async fn test_listener_max_conns() -> Result<()> {
    let listener = ListenConfig {
        max_conns: 2,
        ..Default::default()
    }
    .listen("127.0.0.1:0")
    .await?;

    let first = remote(&listener, &[0]).await?;
    let _second = remote(&listener, &[1]).await?;
    let third = remote(&listener, &[2]).await?;
    let (conn, raddr) = accepted(&listener).await.expect("first conn");
    assert_eq!(raddr, first.local_addr()?);
    assert!(accepted(&listener).await.is_some());
    assert!(
        accepted(&listener).await.is_none(),
        "third remote over the cap"
    );

    // closing a conn frees its slot
    conn.close().await?;
    third.send(&[3]).await?;
    let (conn, raddr) = accepted(&listener).await.expect("third conn");
    assert_eq!(raddr, third.local_addr()?);
    let mut b = [0u8; 8];
    let n = conn.recv(&mut b).await?;
    assert_eq!(&b[..n], &[3]);

    listener.close().await?;
    Ok(())
}

#[tokio::test]
async fn test_listener_conn_buffer_size() -> Result<()> {
    let listener = ListenConfig {
        conn_buffer_size: 100,
        ..Default::default()
    }
    .listen("127.0.0.1:0")
    .await?;

    // each packet takes 32 bytes of the 100, so three fit
    let remote = remote(&listener, &[0; 30]).await?;
    for i in 1..10 {
        remote.send(&[i; 30]).await?;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    let (conn, _) = accepted(&listener).await.expect("conn");
    let mut b = [0u8; 64];
    for i in 0..3 {
        let n = conn.recv(&mut b).await?;
        assert_eq!(&b[..n], &[i; 30]);
    }
    let rest = tokio::time::timeout(Duration::from_millis(100), conn.recv(&mut b)).await;
    assert!(rest.is_err(), "packets over the buffer size were kept");

    // reading made room again
    remote.send(&[10; 30]).await?;
    let n = conn.recv(&mut b).await?;
    assert_eq!(&b[..n], &[10; 30]);

    listener.close().await?;
    Ok(())
}

#[tokio::test]
async fn test_listener_idle_timeout() -> Result<()> {
    let listener = ListenConfig {
        idle_timeout: Duration::from_millis(200),
        ..Default::default()
    }
    .listen("127.0.0.1:0")
    .await?;

    let silent = remote(&listener, &[0]).await?;
    let (silent_conn, _) = accepted(&listener).await.expect("silent conn");
    let active = remote(&listener, &[0]).await?;
    let (active_conn, _) = accepted(&listener).await.expect("active conn");

    let mut b = [0u8; 8];
    for i in 1..10 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        active.send(&[i]).await?;
    }

    // the silent remote's conn is closed and forgotten ...
    assert_eq!(silent_conn.recv(&mut b).await?, 1);
    assert!(is_closed(&silent_conn).await);
    silent.send(&[1]).await?;
    let (_, raddr) = accepted(&listener).await.expect("new conn");
    assert_eq!(raddr, silent.local_addr()?);

    // ... the active one is kept
    for i in 0..10 {
        let n = active_conn.recv(&mut b).await?;
        assert_eq!(&b[..n], &[i]);
    }
    assert!(accepted(&listener).await.is_none());

    listener.close().await?;
    Ok(())
}
