# UDX end-to-end integration examples Rust/Node.js

This folder contains interop/integration examples for the UDX implementations in Node.js and Rust.

## How to run

```
# install Node.js dependencies
npm install

# build Rust code
cargo build

# invoke run script that starts Node.js and Rust processes that will talk to each other
node run.js pipe
```

## Examples

Currently only a single example is implemented: `pipe`. It will send 10 messages from Rust to Node.js and vice-versa.
