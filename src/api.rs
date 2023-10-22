use crate::context::Data;
use anyhow::Context;
use serde::Deserialize;

#[derive(Deserialize)]
pub(crate) struct User {
    pub name: String,
}

#[derive(Deserialize)]
pub(crate) struct Node {
    pub id: String,

    #[serde(rename = "givenName")]
    pub name: String,

    #[serde(rename = "ipAddresses")]
    pub ip_addresses: Vec<String>,

    #[serde(rename = "nodeKey")]
    pub node_key: String,

    pub user: User,
}

#[derive(Deserialize)]
pub(crate) struct RegisterResponse {
    pub machine: Node,
}

#[derive(Deserialize)]
pub(crate) struct ListMachineResponse {
    pub machines: Vec<Node>,
}

pub(crate) async fn get_node_by_nodekey(ctx: &Data, nodekey: &str) -> anyhow::Result<Option<Node>> {
    let resp = ctx
        .headscale
        .get(format!("{}/api/v1/machine", ctx.base_url))
        .send()
        .await
        .context("Failed to list machines")?
        .error_for_status()?;

    let outcome = resp
        .json::<ListMachineResponse>()
        .await
        .context("Failed to list machines")?;

    for machine in outcome.machines {
        if machine.node_key == nodekey {
            return Ok(Some(machine));
        }
    }

    Ok(None)
}

pub(crate) async fn register_node(ctx: &Data, nodekey: &str, user: &str) -> anyhow::Result<Node> {
    let resp = ctx
        .headscale
        .post(format!("{}/api/v1/machine/register", ctx.base_url))
        .query(&[("user", user), ("key", &format!("nodekey:{}", nodekey))])
        .send()
        .await
        .context("Failed to register machine")?;

    let resp = resp.error_for_status()?;

    let outcome = resp
        .json::<RegisterResponse>()
        .await
        .context("Failed to parse response")?;

    Ok(outcome.machine)
}

pub(crate) async fn rename(ctx: &Data, id: &str, name: &str) -> anyhow::Result<Node> {
    let resp = ctx
        .headscale
        .post(format!(
            "{}/api/v1/machine/{}/rename/{name}",
            ctx.base_url, id
        ))
        .send()
        .await
        .context("Failed to rename machine")?
        .error_for_status()?;

    let outcome = resp
        .json::<RegisterResponse>()
        .await
        .context("Failed to parse response")?;

    Ok(outcome.machine)
}

pub(crate) async fn change_user(ctx: &Data, id: &str, user: &str) -> anyhow::Result<Node> {
    let resp = ctx
        .headscale
        .post(format!("{}/api/v1/machine/{}/user", ctx.base_url, id))
        .query(&[("user", user)])
        .send()
        .await
        .context("Failed to change user")?
        .error_for_status()?;

    let outcome = resp
        .json::<RegisterResponse>()
        .await
        .context("Failed to parse response")?;

    Ok(outcome.machine)
}

pub(crate) async fn remove_node(ctx: &Data, id: &str) -> anyhow::Result<()> {
    ctx.headscale
        .delete(format!("{}/api/v1/machine/{}", ctx.base_url, id))
        .send()
        .await
        .context("Failed to delete node")?
        .error_for_status()?;

    Ok(())
}
