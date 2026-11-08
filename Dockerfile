# syntax=docker/dockerfile:1
#
# Image de claude-rotate : binaire statique côté Rust, runtime distroless.
#
#   docker volume create claude-rotate
#   docker run --rm -it -v claude-rotate:/data IMAGE init
#   docker run --rm -it -v claude-rotate:/data IMAGE add-account acct-1
#   docker run --rm -it -v claude-rotate:/data IMAGE add-device mon-portable
#   docker run -d -v claude-rotate:/data -p 127.0.0.1:8484:8484 IMAGE
#
# Important : dans un conteneur, passez "bind": "0.0.0.0" dans
# /data/config.json. Avec la valeur par défaut 127.0.0.1, le proxy n'écoute
# que dans le conteneur et reste injoignable depuis l'hôte. Restreignez
# ensuite l'exposition avec -p <adresse privée>:8484:8484.

FROM rust:1-slim AS builder
WORKDIR /build

# Les caches BuildKit conservent le registre et les artefacts entre deux
# builds : seules les crates modifiées sont recompilées.
COPY Cargo.toml Cargo.lock config.example.json ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked --bin claude-rotate \
    && cp target/release/claude-rotate /usr/local/bin/claude-rotate

# Répertoire de données appartenant à l'utilisateur non root du runtime :
# un volume nommé hérite de ces droits à sa création.
RUN mkdir /data

# distroless/cc fournit la libc, libgcc et les certificats racine, rien d'autre
# (ni shell ni gestionnaire de paquets).
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /usr/local/bin/claude-rotate /usr/local/bin/claude-rotate
COPY --from=builder --chown=65532:65532 /data /data
USER 65532:65532
VOLUME ["/data"]
EXPOSE 8484
ENTRYPOINT ["/usr/local/bin/claude-rotate", "--root", "/data"]
CMD ["serve"]
