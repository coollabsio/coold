mod client;
mod handlers;

pub use client::run;

pub mod proto {
    tonic::include_proto!("coolify.agent.v1");
}
