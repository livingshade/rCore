export ARCH=riscv64
export FILE="riscv64-linux-musl-cross"
export URL="https://more.musl.cc/10/x86_64-linux-musl/riscv64-linux-musl-cross.tgz"
if [ ! -f $FILE.tgz ]; then
    wget $URL
    tar -xzf $FILE.tgz
fi

sudo apt install -y clang 