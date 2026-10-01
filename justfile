# Install optimized for the current CPU.
install:
    cargo install \
        --profile opt \
        --config 'target."cfg(all())".rustflags=["-C", "target-cpu=native"]' \
        --path helix-term \
        --locked

# Build an instrumented native binary, train it, then install using its profile.
install-pgo:
    python3 contrib/pgo.py all

pgo-build:
    python3 contrib/pgo.py build

pgo-train:
    python3 contrib/pgo.py train

pgo-merge:
    python3 contrib/pgo.py merge

pgo-install:
    python3 contrib/pgo.py install
