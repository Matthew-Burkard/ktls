#!/usr/bin/bash

cargo build --release
rm ~/.local/bin/ktls
cp ./target/release/ktls ~/.local/bin/
