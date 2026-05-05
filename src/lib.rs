#[derive(serde::Deserialize, serde::Serialize, Debug, Default, Clone)]
pub /* for tests */ struct Config {
    pub ado_connection_string: String,
    pub trust_cert: Option<bool>,
    pub case_sensitive: Option<bool>,
    pub get_symbols_query: String,
    pub sign: String,
}
