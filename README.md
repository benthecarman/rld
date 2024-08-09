# rld - Rust Lightning Daemon

Rust Lightning Daemon (rld) is an LDK based lightning node with [lnd](https://github.com/lightningnetwork/lnd)'s GRPC
interface. It is currently in development and is not yet ready for production use.

## Usage

You can run rld with the following command, this will start a signet node with a postgres database at
`localhost:5432/rld` and
bitcoind RPC server at `localhost:38332` with the user `user` and password `password`.

```
RUST_LOG="rld=debug,info" cargo run --bin rld -- --pg-url "postgres://username:password@localhost:5432/rld" --bitcoind-rpc-user "user" --bitcoind-rpc-password "password" --network "signet"
```

You can interact with rld using the [lncli](https://github.com/lightningnetwork/lnd) tool.

```
lncli -n signet --macaroonpath ~/.rld/admin.macaroon --tlscertpath ~/.rld/tls.cert --rpcserver localhost:9999 getinfo
```

If you want to pay BOLT12 offers, you can also use [lndk's](https://github.com/lndk-org/lndk) cli tool with rld:

```
lndk-cli -n signet --grpc-port 9999 -m ~/.rld/admin.macaroon --cert-path ~/.rld/tls.cert pay-offer <offer>
```

## Known Limitations

- No support for static channel backups
- No htlc interceptors
- No support for custom tlvs
- No support for custom features
- No support for custom records
- No support for custom channel fees
- Lots of missing GRPC endpoints
- Lots of parameters in GRPC messages are not mapped to LDK types
- Lots of parameters in GRPC messages are unsupported
- Syncing on-chain can be slow
