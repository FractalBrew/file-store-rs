use crate::*;

/// Accesses files in a Backblaze B2 bucket. Included with the feature "b2".
#[derive(Debug)]
pub struct B2Backend {
    settings: FsSettings,
}

impl B2Backend {
    pub fn connect(_settings: FsSettings) -> ConnectFuture {
        unimplemented!();
    }
}

impl FsImpl for B2Backend {
    fn list_files(&self, _path: &FsPath) -> FileListFuture {
        unimplemented!();
    }

    fn get_file(&self, _path: &FsPath) -> FileFuture {
        unimplemented!();
    }

    fn delete_file(&self, _path: &FsPath) -> OperationCompleteFuture {
        unimplemented!();
    }

    fn get_file_stream(&self, _path: &FsPath) -> DataStreamFuture {
        unimplemented!();
    }

    fn write_from_stream(&self, _path: &FsPath, _stream: DataStream) -> OperationCompleteFuture {
        unimplemented!();
    }
}
