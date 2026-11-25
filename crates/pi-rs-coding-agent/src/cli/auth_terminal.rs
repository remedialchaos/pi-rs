//! Line-oriented terminal I/O for auth commands. Secret text is never echoed.

use std::io::{self, IsTerminal, Write};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pi_rs_agent::CancellationToken;
use pi_rs_ai::{Error, Result};
use pi_rs_tui::keys::{decode_printable_key, is_key_release_data, matches_key_data};
use pi_rs_tui::terminal::{ProcessTerminal, TerminalEvent};

use crate::utils::open_browser::try_open_web_url;
use crate::utils::web_url::web_url;

pub type BrowserOpener = Arc<dyn Fn(&str) -> std::result::Result<(), String> + Send + Sync>;

#[async_trait]
pub trait LoginTerminal: Send + Sync {
    fn interactive(&self) -> bool;
    fn write(&self, line: &str);
    fn error(&self, line: &str);
    fn open_url(&self, url: &str);
    async fn prompt(
        &self,
        message: &str,
        secret: bool,
        signal: &CancellationToken,
    ) -> Result<String>;
    async fn settle(&self);
}

pub struct ProcessLoginTerminal {
    interrupt: CancellationToken,
    prompt_lock: Arc<tokio::sync::Mutex<()>>,
    browser_opener: BrowserOpener,
}

impl ProcessLoginTerminal {
    pub fn new(interrupt: CancellationToken) -> Self {
        Self {
            interrupt,
            prompt_lock: Arc::default(),
            browser_opener: Arc::new(try_open_web_url),
        }
    }

    pub fn with_browser_opener(mut self, opener: BrowserOpener) -> Self {
        self.browser_opener = opener;
        self
    }
}

#[async_trait]
impl LoginTerminal for ProcessLoginTerminal {
    fn interactive(&self) -> bool {
        io::stdin().is_terminal() && io::stdout().is_terminal()
    }

    fn write(&self, line: &str) {
        println!("{line}");
    }

    fn error(&self, line: &str) {
        eprintln!("{line}");
    }

    fn open_url(&self, url: &str) {
        let Some(target) = web_url(url) else {
            return;
        };
        let _ = (self.browser_opener)(&target);
    }

    async fn prompt(
        &self,
        message: &str,
        secret: bool,
        signal: &CancellationToken,
    ) -> Result<String> {
        let lock = tokio::select! {
            biased;
            _ = signal.cancelled() => return Err(Error::Aborted),
            _ = self.interrupt.cancelled() => return Err(Error::Aborted),
            lock = self.prompt_lock.clone().lock_owned() => lock,
        };
        let prompt = signal.child_token();
        // A browser callback may drop this prompt future without cancelling the login.
        // The guard still stops the worker and restores the terminal before another prompt starts.
        let _cancel_prompt = prompt.clone().drop_guard();
        let interrupt = self.interrupt.clone();
        let message = message.to_owned();
        tokio::task::spawn_blocking(move || {
            let _lock = lock;
            read_line(&message, secret, &prompt, &interrupt)
        })
        .await
        .map_err(|error| Error::Auth(error.to_string()))?
    }

    async fn settle(&self) {
        // Acquiring the lock waits for any cancelled reader to restore terminal state.
        let _guard = self.prompt_lock.lock().await;
    }
}

fn read_line(
    message: &str,
    secret: bool,
    prompt: &CancellationToken,
    interrupt: &CancellationToken,
) -> Result<String> {
    if prompt.is_cancelled() || interrupt.is_cancelled() {
        return Err(Error::Aborted);
    }
    let mut terminal = ProcessTerminal::new();
    terminal.start()?;
    let result = read_input(&mut terminal, message, secret, prompt, interrupt);
    let restore = terminal.stop();
    write_prompt("\n")?;
    restore?;
    result
}

fn write_prompt(text: &str) -> io::Result<()> {
    let mut out = io::stdout().lock();
    out.write_all(text.as_bytes())?;
    out.flush()
}

fn read_input(
    terminal: &mut ProcessTerminal,
    message: &str,
    secret: bool,
    prompt: &CancellationToken,
    interrupt: &CancellationToken,
) -> Result<String> {
    write_prompt(message)?;
    let mut value = String::new();
    while !prompt.is_cancelled() && !interrupt.is_cancelled() {
        for event in terminal.poll_events(Duration::from_millis(30))? {
            let TerminalEvent::Input(data) = event else {
                continue;
            };
            if is_key_release_data(&data) {
                continue;
            }
            if data == "\x03"
                || matches_key_data(&data, "ctrl+c")
                || (!secret
                    && value.is_empty()
                    && (data == "\x04" || matches_key_data(&data, "ctrl+d")))
            {
                interrupt.cancel();
                return Err(Error::Aborted);
            }
            if matches!(data.as_str(), "\r" | "\n") || matches_key_data(&data, "enter") {
                return Ok(value);
            }
            if matches!(data.as_str(), "\x08" | "\x7f") || matches_key_data(&data, "backspace") {
                if value.pop().is_some() && !secret {
                    write_prompt("\x08 \x08")?;
                }
                continue;
            }
            let decoded;
            let data = if let Some(pasted) = data
                .strip_prefix("\x1b[200~")
                .and_then(|data| data.strip_suffix("\x1b[201~"))
            {
                pasted
            } else if data.starts_with('\x1b') {
                decoded = decode_printable_key(&data);
                let Some(text) = decoded.as_deref() else {
                    continue;
                };
                text
            } else {
                data.as_str()
            };
            let start = value.len();
            value.extend(data.chars().filter(|c| !c.is_control()));
            if !secret {
                write_prompt(&value[start..])?;
            }
        }
    }
    Err(Error::Aborted)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn process_login_terminal_injectable_opener_validates_web_url()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let opened = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&opened);
        let terminal = ProcessLoginTerminal::new(CancellationToken::new()).with_browser_opener(
            Arc::new(move |url| {
                seen.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(url.to_owned());
                Ok(())
            }),
        );
        terminal.open_url("javascript:alert(1)");
        terminal.open_url("file:///etc/passwd");
        terminal.open_url("data:text/html,hi");
        terminal.open_url("HTTPS://Example.COM/oauth/authorize?state=1");

        let urls = opened.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            *urls,
            vec!["https://example.com/oauth/authorize?state=1".to_owned()]
        );
        Ok(())
    }
}
