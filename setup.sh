export ARCH=riscv64
export FILE="riscv64-linux-musl-cross"
export URL="https://musl.cc/$FILE.tgz"
if [ ! -f $FILE.tgz ]; then
    wget $URL
    tar -xzf $FILE.tgz
fi
export PATH=$PATH:$PWD/$FILE/bin%