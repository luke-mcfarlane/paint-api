FROM rust:latest as builder
WORKDIR /usr/src/app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
COPY --from=builder /usr/src/app/target/release/paint-server /usr/local/bin/
EXPOSE 8080
CMD ["paint-server"]