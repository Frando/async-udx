# async-udx

udx is reliable, multiplex, and congestion controlled streams over udp. This crate is a port of [libudx](https://github.com/hyperswarm/libudx/) to Rust. It uses the Tokio async runtime.

## Status

This is an alpha release. The wire protocol works and is compatible to the Node.js version.
It misses testing, some congestion control features and does not implement the network interface detection features of libudx.
Perfomance is not yet ideal, but it should be en par or faster to the Node.js implementation (minus some congestion control optimizations).

## Usage

See [this example]('./examples/bench.rs') for an example.

## End-to-end example

The repo includes an [end to end example script](end-to-end/README.md) that runs the protocol between Rust and Node.js implementations.
