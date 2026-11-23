use super::*;
use pi_rs_durable::env::LocalExecutionEnv as DurableLocal;

/// Local durable operations with recursive listing supplied by the caller's daemon.
pub struct LocalExecutionEnv {
    local: DurableLocal,
    connection: Arc<Connection>,
}
impl LocalExecutionEnv {
    pub fn new(options: LocalEnvOptions, connection: Arc<Connection>) -> Self {
        Self {
            local: DurableLocal::with_options(options),
            connection,
        }
    }
}
#[async_trait]
impl FileSystem for LocalExecutionEnv {
    fn id(&self) -> &str {
        self.local.id()
    }
    fn cwd(&self) -> &str {
        self.local.cwd()
    }
    async fn absolute_path(&self, path: &str, context: &Context) -> Result<String, FileError> {
        self.local.absolute_path(path, context).await
    }
    async fn join_path(&self, parts: &[&str], context: &Context) -> Result<String, FileError> {
        self.local.join_path(parts, context).await
    }
    async fn read_text_file(&self, path: &str, context: &Context) -> Result<String, FileError> {
        self.local.read_text_file(path, context).await
    }
    async fn open_text_line_reader(
        &self,
        path: &str,
        context: &Context,
    ) -> Result<Box<dyn TextLineReader>, FileError> {
        self.local.open_text_line_reader(path, context).await
    }
    async fn read_binary_file(&self, path: &str, context: &Context) -> Result<Vec<u8>, FileError> {
        self.local.read_binary_file(path, context).await
    }
    async fn open_binary_reader(
        &self,
        path: &str,
        options: Option<OpenBinaryOptions>,
        context: &Context,
    ) -> Result<Box<dyn BinaryReader>, FileError> {
        self.local.open_binary_reader(path, options, context).await
    }
    async fn write_file(
        &self,
        path: &str,
        content: &[u8],
        context: &Context,
    ) -> Result<(), FileError> {
        self.local.write_file(path, content, context).await
    }
    async fn append_file(
        &self,
        path: &str,
        content: &[u8],
        context: &Context,
    ) -> Result<(), FileError> {
        self.local.append_file(path, content, context).await
    }
    async fn truncate_file(
        &self,
        path: &str,
        size: u64,
        context: &Context,
    ) -> Result<(), FileError> {
        self.local.truncate_file(path, size, context).await
    }
    async fn flush_file(&self, path: &str, context: &Context) -> Result<(), FileError> {
        self.local.flush_file(path, context).await
    }
    async fn rename_file(
        &self,
        source: &str,
        destination: &str,
        context: &Context,
    ) -> Result<(), FileError> {
        self.local.rename_file(source, destination, context).await
    }
    async fn file_info(&self, path: &str, context: &Context) -> Result<FileInfo, FileError> {
        self.local.file_info(path, context).await
    }
    async fn list_dir(&self, path: &str, context: &Context) -> Result<Vec<FileInfo>, FileError> {
        self.local.list_dir(path, context).await
    }
    async fn list_files(
        &self,
        path: &str,
        options: Option<ListFilesOptions>,
        context: &Context,
    ) -> Result<FileListing, FileError> {
        list_files_over(
            &self.connection,
            &self.local.absolute_path(path, context).await?,
            options,
            context,
        )
        .await
    }
    async fn open_dir_reader(
        &self,
        path: &str,
        context: &Context,
    ) -> Result<Box<dyn DirReader>, FileError> {
        self.local.open_dir_reader(path, context).await
    }
    async fn watch(
        &self,
        targets: &[WatchTarget],
        on_change: WatchCallback,
        context: &Context,
    ) -> Result<Box<dyn FileWatcher>, FileError> {
        self.local.watch(targets, on_change, context).await
    }
    async fn canonical_path(&self, path: &str, context: &Context) -> Result<String, FileError> {
        self.local.canonical_path(path, context).await
    }
    async fn create_dir(
        &self,
        path: &str,
        options: Option<CreateDirOptions>,
        context: &Context,
    ) -> Result<(), FileError> {
        self.local.create_dir(path, options, context).await
    }
    async fn remove(
        &self,
        path: &str,
        options: Option<RemoveOptions>,
        context: &Context,
    ) -> Result<(), FileError> {
        self.local.remove(path, options, context).await
    }
    async fn create_temp_dir(
        &self,
        prefix: Option<&str>,
        context: &Context,
    ) -> Result<String, FileError> {
        self.local.create_temp_dir(prefix, context).await
    }
    async fn create_temp_file(
        &self,
        options: Option<TempFileOptions<'_>>,
        context: &Context,
    ) -> Result<String, FileError> {
        self.local.create_temp_file(options, context).await
    }
}
#[async_trait]
impl Shell for LocalExecutionEnv {
    async fn exec(
        &self,
        command: ShellCommand<'_>,
        options: Option<ShellExecOptions<'_>>,
        context: &Context,
    ) -> Result<ShellExecResult, ExecutionError> {
        self.local.exec(command, options, context).await
    }
    async fn cleanup(&self, context: &Context) {
        self.local.cleanup(context).await
    }
}
