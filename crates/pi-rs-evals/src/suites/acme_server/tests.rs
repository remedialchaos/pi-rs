use super::*;

#[tokio::test]
async fn stream_probe_requires_resolved_key_and_records_the_exact_prompt()
-> Result<(), Box<dyn std::error::Error>> {
    let server = Server::start(true)?;
    for (key, prompt, expected, valid) in [
        ("wrong", "Reply with ACME_STREAM_OK.", "401", false),
        ("resolved-stream-key", "Other prompt", "200", false),
        (
            "resolved-stream-key",
            "Reply with ACME_STREAM_OK.",
            "200",
            true,
        ),
    ] {
        let payload=json!({"model":server.model(),"messages":[{"role":"user","content":prompt}],"stream":true}).to_string();
        let address = server
            .origin
            .strip_prefix("http://")
            .ok_or("Malformed fixture origin")?;
        let mut socket = TcpStream::connect(address).await?;
        socket.write_all(format!("POST /generate HTTP/1.1\r\nHost: {address}\r\nx-acme-key: {key}\r\nContent-Length: {}\r\n\r\n{payload}",payload.len()).as_bytes()).await?;
        let mut response = String::new();
        socket.read_to_string(&mut response).await?;
        assert!(
            response.starts_with(&format!("HTTP/1.1 {expected}")),
            "{response}"
        );
        assert_eq!(server.valid(), valid);
        if valid {
            let (_, body) = response
                .split_once("\r\n\r\n")
                .ok_or("missing response body")?;
            let events = body
                .lines()
                .map(serde_json::from_str::<Value>)
                .collect::<Result<Vec<_>, _>>()?;
            assert_eq!(
                events[2],
                json!({"type":"usage","input_tokens":4,"output_tokens":3})
            );
            assert_eq!(events[3], json!({"type":"done","reason":"stop"}));
        }
    }
    Ok(())
}
