
cargo run --release \
  -- --identity ./test-keypair.json \
  --vote-account ./test-keypair.json \
  --rpc-port 8899 \
  --entrypoint entrypoint.devnet.solana.com:8001 \
  --limit-ledger-size \
  --log ./solana-validator.log \
  --no-voting \
  --no-os-network-limits-test \
  --skip-poh-verify \
  --no-incremental-snapshots \
  --no-poh-speed-test