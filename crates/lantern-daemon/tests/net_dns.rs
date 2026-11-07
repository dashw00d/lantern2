use lantern_daemon::net::dns::{Dns, DnsConfig, respond};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpSocket, UdpSocket},
    time::{Duration, timeout},
};
use tokio_util::sync::CancellationToken;

fn query(name: &str, typ: u16) -> Vec<u8> {
    let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
    for label in name.split('.') {
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.push(0);
    q.extend_from_slice(&typ.to_be_bytes());
    q.extend_from_slice(&1u16.to_be_bytes());
    q
}
fn count(p: &[u8], n: usize) -> u16 {
    u16::from_be_bytes([p[n], p[n + 1]])
}
#[test]
fn net_dns_wire_policy_and_malformed_inputs() {
    let q = query("App.Glow", 1);
    let r = respond(&q, "glow", false).unwrap();
    assert_eq!(&r[0..2], &q[0..2]);
    assert_eq!(count(&r, 6), 1);
    assert_eq!(&r[12..q.len()], &q[12..]);
    assert_eq!(&r[r.len() - 4..], &[127, 0, 0, 1]);
    assert_eq!(
        count(&respond(&query("app.glow", 28), "glow", false).unwrap(), 6),
        0
    );
    let v6 = respond(&query("app.glow", 28), "glow", true).unwrap();
    assert_eq!(count(&v6, 6), 1);
    assert_eq!(&v6[v6.len() - 16..], &std::net::Ipv6Addr::LOCALHOST.octets());
    assert_eq!(count(&respond(&query("glow", 1), "glow", false).unwrap(), 6), 0);
    assert_eq!(
        count(&respond(&query("app.glow", 65), "glow", false).unwrap(), 6),
        0
    );
    assert_eq!(
        respond(&query("example.com", 1), "glow", false).unwrap()[3] & 15,
        5
    );
    let mut pointer = query("app.glow", 1);
    pointer[12] = 0xc0;
    pointer[13] = 0x0c;
    assert_eq!(respond(&pointer, "glow", false).unwrap()[3] & 15, 1);
    let mut edns = query("app.glow", 1);
    edns[11] = 1;
    edns.extend_from_slice(&[0, 0, 41, 4, 208, 0, 0, 0, 0, 0, 0]);
    let response = respond(&edns, "glow", false).unwrap();
    assert_eq!(count(&response, 10), 1);
    assert_eq!(
        &response[response.len() - 11..],
        &[0, 0, 41, 4, 208, 0, 0, 0, 0, 0, 0]
    );
    let mut seed = 0x8ab6_4913_u64;
    for len in 0..700 {
        for _ in 0..30 {
            let mut bytes = vec![0; len];
            for b in &mut bytes {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                *b = (seed >> 32) as u8;
            }
            let _ = respond(&bytes, "glow", true);
        }
    }
}
#[tokio::test]
async fn net_dns_real_udp_and_tcp_queries() {
    let cancel = CancellationToken::new();
    let cfg = DnsConfig {
        listen: "127.0.0.1:39994".parse().unwrap(),
        tld: "glow".into(),
        ipv6: false,
    };
    let task = tokio::spawn(Dns::serve(cfg, cancel.clone()));
    let udp = UdpSocket::bind("127.0.0.1:39995").await.unwrap();
    let q = query("app.glow", 1);
    let mut bytes = [0u8; 512];
    let mut attempts = 0;
    let reply = loop {
        attempts += 1;
        assert!(attempts <= 20, "DNS did not answer");
        udp.send_to(&q, "127.0.0.1:39994").await.unwrap();
        if let Ok(Ok((n, _))) = timeout(Duration::from_millis(100), udp.recv_from(&mut bytes)).await {
            break bytes[..n].to_vec();
        }
        assert!(!task.is_finished(), "DNS server failed to start");
    };
    assert_eq!(&reply[reply.len() - 4..], &[127, 0, 0, 1]);
    let tcp_socket = TcpSocket::new_v4().unwrap();
    tcp_socket.bind("127.0.0.1:39996".parse().unwrap()).unwrap();
    let mut tcp = tcp_socket
        .connect("127.0.0.1:39994".parse().unwrap())
        .await
        .unwrap();
    tcp.write_u16(q.len() as u16).await.unwrap();
    tcp.write_all(&q).await.unwrap();
    let len = timeout(Duration::from_secs(2), tcp.read_u16())
        .await
        .unwrap()
        .unwrap() as usize;
    let mut response = vec![0; len];
    tcp.read_exact(&mut response).await.unwrap();
    assert_eq!(response, reply);
    cancel.cancel();
    timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
