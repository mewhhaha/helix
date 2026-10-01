# Install optimized for the current CPU.
install:
    cargo install \
        --profile opt \
        --config 'target."cfg(all())".rustflags=["-C", "target-cpu=native"]' \
        --path helix-term \
        --locked
