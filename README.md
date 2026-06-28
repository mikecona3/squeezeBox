<h1>Squeezebox</h1>
<p>A simple compression algorithm</p>

<h2>How to run it:</h2>

# system deps (one-time)
apt install liblzma-dev libbz2-dev

# build
cargo build --release

# use it
./target/release/max_compress compress myfile myfile.mcz
./target/release/max_compress decompress myfile.mcz myfile_restored
