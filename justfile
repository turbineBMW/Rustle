# Rustle — task runner. `just` with no args lists recipes.

# Build the app (debug). Blueprints, resources and the schema compile via build.rs.
build:
    cargo build

# Build and run from the checkout. RUSTLE_LOG=debug for verbose logging.
run *ARGS:
    cargo run -p rustle -- {{ARGS}}

# clippy (warnings are errors) + rustfmt check + unit tests.
check:
    cargo clippy --workspace --all-targets -- -D warnings
    cargo fmt --all -- --check
    cargo test --workspace

fmt:
    cargo fmt --all

# Install the release build for the current user (PREFIX=/usr for system-wide).
install:
    sh install.sh

clean:
    cargo clean
