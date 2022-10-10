# udx-udp

Optimized UDP socket wrapper. Copied from [quinn-udp](https://github.com/quinn-rs/quinn) at revision `16769e3fa9c8f366d6cdb49808e0d4fa83b88930`.

Changes:
* Move `Transmit` and `EcnCodepoint` structs from `quinn-proto` into `proto` module of this crate to not depend on `quinn-proto`.
* No other changes.
