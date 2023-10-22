use kube::Client;

pub(crate) struct Data {
    pub client: Client,
    pub base_url: String,
    pub headscale: reqwest::Client,
    pub default_user: Option<String>,
    pub load_balancer_class: String,
    pub storage_class: String,
    pub tailscale_image: String,
}
