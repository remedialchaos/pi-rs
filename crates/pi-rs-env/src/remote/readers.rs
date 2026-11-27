use super::*;

pub(super) fn file_info(path: &str, j: &Value, separator: &str) -> Result<FileInfo, FileError> {
    let kind = match j["kind"].as_str() {
        Some("file") => FileKind::File,
        Some("directory") => FileKind::Directory,
        Some("symlink") => FileKind::Symlink,
        _ => {
            return Err(failure(
                FileErrorCode::Invalid,
                "Unsupported file type",
                path,
            ));
        }
    };
    let invalid = || failure(FileErrorCode::Unknown, "Invalid daemon stat", path);
    let name_path = if separator == "\\"
        && path.as_bytes().get(1) == Some(&b':')
        && path.as_bytes()[0].is_ascii_alphabetic()
    {
        &path[2..]
    } else {
        path
    };
    Ok(FileInfo {
        name: name_path
            .trim_end_matches(separator)
            .rsplit(separator)
            .next()
            .unwrap_or_default()
            .into(),
        path: path.into(),
        kind,
        size: j["size"].as_u64().ok_or_else(invalid)?,
        mtime_ms: j["mtimeSec"].as_f64().ok_or_else(invalid)? * 1000.
            + j["mtimeNsec"].as_f64().ok_or_else(invalid)? / 1e6,
    })
}
pub(super) struct Handle {
    connection: Arc<Connection>,
    id: Option<u64>,
    session: u64,
    pub path: String,
    pub separator: String,
}
impl Handle {
    pub async fn acquire(
        c: &Arc<Connection>,
        path: &str,
        op: &'static str,
        fields: Value,
        options: RequestOptions,
    ) -> Result<(Self, Value), FileError> {
        let connection = c.clone();
        let name = path.to_owned();
        // Construct the owner before crossing the await boundary. If the caller
        // drops the future, the worker's unclaimed result still closes the handle.
        transport(path, move || {
            let separator = connection.info()?.separator;
            let reply = connection.request(op, fields, options)?;
            let id = reply.json["handle"].as_u64().ok_or_else(|| {
                RemoteError::new(&json!({"code":"unknown","message":"Invalid daemon handle"}))
            })?;
            Ok((
                Self {
                    connection,
                    id: Some(id),
                    path: name,
                    session: reply.session,
                    separator,
                },
                reply.json,
            ))
        })
        .await
    }
    pub async fn send(
        &self,
        op: &'static str,
        mut j: Value,
        payload: Vec<u8>,
    ) -> Result<Reply, FileError> {
        j["handle"] = json!(self.id.ok_or_else(|| failure(
            FileErrorCode::Invalid,
            "Reader is closed",
            &self.path
        ))?);
        request(
            &self.connection,
            op,
            j,
            RequestOptions {
                payload,
                session: Some(self.session),
                ..Default::default()
            },
            &self.path,
        )
        .await
    }
    pub(super) async fn submit_chunk(
        &self,
        chunk: &[u8],
    ) -> Result<std::sync::mpsc::Receiver<Result<Reply, RemoteError>>, FileError> {
        let connection = self.connection.clone();
        let session = self.session;
        let id = self.id;
        let bytes = chunk.to_vec();
        transport(&self.path, move || {
            connection.submit(session, "writeChunk", json!({"handle": id}), bytes)
        })
        .await
    }

    fn take_close(&mut self) -> Option<impl FnOnce() + Send + 'static + use<>> {
        let id = self.id.take()?;
        let c = self.connection.clone();
        let session = self.session;
        Some(move || {
            let _ = c.request(
                "close",
                json!({"handle":id}),
                RequestOptions {
                    session: Some(session),
                    ..Default::default()
                },
            );
        })
    }
    pub(super) async fn release(&mut self) {
        if let Some(close) = self.take_close() {
            let _ = tokio::task::spawn_blocking(close).await;
        }
    }
    pub async fn range(
        &self,
        offset: u64,
        length: u64,
        ctx: &Context,
    ) -> Result<Vec<u8>, FileError> {
        check(ctx, &self.path)?;
        if self.id.is_none() {
            return Err(failure(
                FileErrorCode::Invalid,
                "Binary reader is closed",
                &self.path,
            ));
        }
        if offset > MAX_SAFE || length > MAX_SAFE {
            return Err(failure(
                FileErrorCode::Invalid,
                "Offset and length must be non-negative safe integers",
                &self.path,
            ));
        }
        let mut bytes = Vec::new();
        while (bytes.len() as u64) < length {
            let used = bytes.len() as u64;
            let chunk = self
                .send(
                    "pread",
                    json!({"offset":offset+used,"length":(length-used).min(256*1024)}),
                    vec![],
                )
                .await?
                .payload;
            check(ctx, &self.path)?;
            if chunk.is_empty() {
                break;
            }
            bytes.extend(chunk);
        }
        Ok(bytes)
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        if let Some(close) = self.take_close() {
            detached_cleanup(close);
        }
    }
}
#[async_trait]
impl BinaryReader for Handle {
    async fn info(&self, ctx: &Context) -> Result<FileInfo, FileError> {
        check(ctx, &self.path)?;
        file_info(
            &self.path,
            &self.send("fstat", json!({}), vec![]).await?.json,
            &self.separator,
        )
    }
    async fn read(
        &mut self,
        offset: u64,
        length: u64,
        ctx: &Context,
    ) -> Result<Vec<u8>, FileError> {
        self.range(offset, length, ctx).await
    }
    async fn scan_lines(
        &mut self,
        opts: ScanLinesOptions,
        ctx: &Context,
    ) -> Result<LineScan, FileError> {
        check(ctx, &self.path)?;
        let id = self.id.ok_or_else(|| {
            failure(
                FileErrorCode::Invalid,
                "Binary reader is closed",
                &self.path,
            )
        })?;
        if opts.start_line > MAX_SAFE
            || opts
                .end_line
                .is_some_and(|n| n > MAX_SAFE || n <= opts.start_line)
        {
            return Err(failure(
                FileErrorCode::Invalid,
                "Invalid line range",
                &self.path,
            ));
        }
        let controller = Arc::new(crate::AbortController::new());
        let cancel = controller.clone();
        let _subscription = ctx
            .abort_signal()
            .map(|s| s.subscribe(move |_| cancel.abort()));
        let connection = self.connection.clone();
        let session = self.session;
        let mut fields = json!(opts);
        fields["handle"] = json!(id);
        let r = request(
            &connection,
            "scanLines",
            fields,
            RequestOptions {
                signal: Some(controller.signal()),
                session: Some(session),
                ..Default::default()
            },
            &self.path,
        )
        .await?;
        check(ctx, &self.path)?;
        serde_json::from_value(r.json)
            .map_err(|e| failure(FileErrorCode::Unknown, e.to_string(), &self.path))
    }
    async fn close(&mut self, _: &Context) {
        self.release().await;
    }
}
struct LineBuffer {
    buffer: String,
    start: usize,
    scanned: usize,
    decoder: StreamDecoder,
    #[cfg(test)]
    scanned_bytes: usize,
}

impl LineBuffer {
    fn new() -> Self {
        Self {
            buffer: String::new(),
            start: 0,
            scanned: 0,
            decoder: StreamDecoder::new(),
            #[cfg(test)]
            scanned_bytes: 0,
        }
    }

    fn next_line(&mut self) -> Option<TextLine> {
        let slice = &self.buffer.as_bytes()[self.scanned..];
        #[cfg(test)]
        {
            self.scanned_bytes += slice.len();
        }
        let newline = slice.iter().position(|byte| *byte == b'\n')?;
        let end = self.scanned + newline;
        let text = self.buffer[self.start..end].to_owned();
        self.start = end + 1;
        self.scanned = self.start;
        Some(TextLine {
            text,
            terminated: true,
        })
    }

    fn finish_eof(&mut self) -> Option<TextLine> {
        if self.start == self.buffer.len() {
            return None;
        }
        let mut text = std::mem::take(&mut self.buffer);
        if self.start != 0 {
            text.drain(..self.start);
        }
        self.start = 0;
        self.scanned = 0;
        Some(TextLine {
            text,
            terminated: false,
        })
    }

    fn prepare_read(&mut self) {
        if self.start != 0 {
            self.buffer.drain(..self.start);
            self.start = 0;
        }
        self.scanned = self.buffer.len();
    }

    fn push_chunk(&mut self, chunk: Option<&[u8]>) {
        self.buffer.push_str(&self.decoder.decode(chunk));
    }

    fn clear(&mut self) {
        self.buffer.clear();
        self.start = 0;
        self.scanned = 0;
    }
}

pub(super) struct Lines {
    handle: Handle,
    lines: LineBuffer,
    offset: u64,
    eof: bool,
}
impl Lines {
    pub fn new(handle: Handle) -> Self {
        Self {
            handle,
            lines: LineBuffer::new(),
            offset: 0,
            eof: false,
        }
    }
}
#[async_trait]
impl TextLineReader for Lines {
    async fn read_line(&mut self, ctx: &Context) -> Result<Option<TextLine>, FileError> {
        check(ctx, &self.handle.path)?;
        if self.handle.id.is_none() {
            return Err(failure(
                FileErrorCode::Invalid,
                "Text line reader is closed",
                &self.handle.path,
            ));
        }
        loop {
            if let Some(line) = self.lines.next_line() {
                return Ok(Some(line));
            }
            if self.eof {
                return Ok(self.lines.finish_eof());
            }
            self.lines.prepare_read();
            let chunk = self
                .handle
                .send(
                    "pread",
                    json!({"offset":self.offset,"length":65536}),
                    vec![],
                )
                .await?
                .payload;
            check(ctx, &self.handle.path)?;
            self.offset += chunk.len() as u64;
            self.eof = chunk.is_empty();
            self.lines.push_chunk((!self.eof).then_some(&chunk));
        }
    }
    async fn close(&mut self, _: &Context) {
        self.handle.release().await;
        self.lines.clear();
    }
}
pub(super) struct Directory {
    pub handle: Handle,
    pub done: bool,
}
impl Directory {
    pub fn new(handle: Handle) -> Self {
        Self {
            handle,
            done: false,
        }
    }
    pub async fn page(
        &mut self,
        max: u64,
        ctx: &Context,
    ) -> Result<Vec<(Vec<u8>, FileInfo)>, FileError> {
        let path = &self.handle.path;
        check(ctx, path)?;
        if self.handle.id.is_none() {
            return Err(failure(
                FileErrorCode::Invalid,
                "Directory reader is closed",
                path,
            ));
        }
        if max == 0 || max > MAX_SAFE {
            return Err(failure(
                FileErrorCode::Invalid,
                "maxEntries must be a positive safe integer",
                path,
            ));
        }
        let mut entries = Vec::new();
        while !self.done && (entries.len() as u64) < max {
            let j = self
                .handle
                .send(
                    "readdir",
                    json!({"max":(max-entries.len() as u64).min(1000)}),
                    vec![],
                )
                .await?
                .json;
            check(ctx, path)?;
            for entry in j["entries"]
                .as_array()
                .ok_or_else(|| failure(FileErrorCode::Unknown, "Invalid readdir response", path))?
            {
                let name = field(entry, "name", path)?;
                let sep = &self.handle.separator;
                let p = format!("{}{sep}{name}", path.trim_end_matches(sep.as_str()));
                if let Some(e) = entry.get("error") {
                    if e["code"] == "ENOENT" {
                        continue;
                    }
                    return Err(translate(RemoteError::new(e), &p));
                }
                if entry["info"]["kind"] == "other" {
                    continue;
                }
                let raw = match entry.get("raw") {
                    Some(bytes) => serde_json::from_value(bytes.clone())
                        .map_err(|e| failure(FileErrorCode::Unknown, e.to_string(), &p))?,
                    None => name.as_bytes().to_vec(),
                };
                entries.push((raw, file_info(&p, &entry["info"], sep)?));
            }
            self.done = j["done"] == true;
        }
        Ok(entries)
    }
}
#[async_trait]
impl DirReader for Directory {
    async fn next(&mut self, max: u64, ctx: &Context) -> Result<DirPage, FileError> {
        let entries = self
            .page(max, ctx)
            .await?
            .into_iter()
            .map(|(_, info)| info)
            .collect();
        Ok(DirPage {
            entries,
            done: self.done,
        })
    }
    async fn close(&mut self, _: &Context) {
        self.handle.release().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_large_chunked_lines_in_linear_time() {
        let long_line = "a".repeat(1024 * 1024);
        let mut data = Vec::with_capacity(long_line.len() + 32);
        data.extend_from_slice(long_line.as_bytes());
        data.extend_from_slice(b"\nsecond\nlast");
        let mut chunks = data.chunks(65_536);
        let mut eof = false;
        let mut reader = LineBuffer::new();
        let mut out = Vec::new();
        loop {
            if let Some(line) = reader.next_line() {
                out.push(line);
                continue;
            }
            if eof {
                if let Some(line) = reader.finish_eof() {
                    out.push(line);
                }
                break;
            }
            reader.prepare_read();
            match chunks.next() {
                Some(chunk) => reader.push_chunk(Some(chunk)),
                None => {
                    eof = true;
                    reader.push_chunk(None);
                }
            }
        }
        assert_eq!(
            out,
            [
                TextLine {
                    text: long_line,
                    terminated: true,
                },
                TextLine {
                    text: "second".into(),
                    terminated: true,
                },
                TextLine {
                    text: "last".into(),
                    terminated: false,
                },
            ]
        );
        assert!(
            reader.scanned_bytes <= 2 * data.len(),
            "scanned {} bytes for {} byte input",
            reader.scanned_bytes,
            data.len()
        );
    }
}
