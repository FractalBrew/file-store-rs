# file-store

[![Build Status](https://travis-ci.org/FractalBrew/file-store-rs.svg?branch=master)](https://travis-ci.org/FractalBrew/file-store-rs)
[![Coverage Status](https://coveralls.io/repos/github/FractalBrew/file-store-rs/badge.svg?branch=master)](https://coveralls.io/github/FractalBrew/file-store-rs?branch=master)

`file-store` is a Rust library providing asynchronous file storage. The files may be hosted locally or remotely. Different backends provide access to different storage systems including the local filesystem and storage in various cloud providers.

The public API for reading and writing files is identical regardless of the chosen storage backend..

## Backends

The currently provided backends are:

* FileBackend allows accessing files within a directory on the local computer.
* B2Backend allows accessing files stored on Backblaze B2.

It is possible to choose which backends are included in the library based on cargo features. The default is to include all backends and so in order to reduce the set you must disable the default features and then list all of the backends you want.