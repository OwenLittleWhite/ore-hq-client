## 1
sudo apt-get update
sudo apt-get install musl-tools

## 2
rustup target add x86_64-unknown-linux-musl

## 3
export CC=musl-gcc
export CXX=musl-g++
export OPENSSL_STATIC=1
cargo build --release --target x86_64-unknown-linux-musl
