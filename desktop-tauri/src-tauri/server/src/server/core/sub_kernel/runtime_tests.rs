use super::*;

fn provider(id: &str) -> ProviderInput {
    ProviderInput {
        id: id.to_string(),
        name: format!("订阅 {id}"),
        url: "https://example.invalid/subscription".to_string(),
        kind: crate::server::core::subscriptions::KIND_URL.to_string(),
        content: String::new(),
    }
}

/// 本地配置文件形态的 provider（`type: file`，无 interval）
fn file_provider(id: &str) -> ProviderInput {
    ProviderInput {
        id: id.to_string(),
        name: format!("本地配置 {id}"),
        url: String::new(),
        kind: crate::server::core::subscriptions::KIND_FILE.to_string(),
        content: "proxies: [{name: node, type: ss}]".to_string(),
    }
}

fn node(provider_id: &str, name: &str, port: u16) -> NodeTarget {
    NodeTarget {
        provider_id: provider_id.to_string(),
        sub_name: format!("订阅 {provider_id}"),
        node: name.to_string(),
        port: Some(port),
    }
}

fn config(providers: &[ProviderInput], nodes: &[NodeTarget]) -> Value {
    let defs = nodes.iter().map(|target| (node_key(&target.provider_id, &target.node), json!({"name": target.node, "type": "ss"}))).collect();
    let yaml = build_config(providers, 19090, "synthetic-controller-secret", 19080, nodes, &defs)
        .expect("合成配置应能序列化");
    serde_yaml::from_str(&yaml).expect("输出应为有效 YAML")
}

/// 对应固定 mihomo 的全局出口查找，再按 use/filter 选择 provider 内节点。
/// 这里只验证配置关系，不模拟真实网络连通性。
fn matching_nodes<'a>(
    config: &Value,
    listener: &Value,
    nodes: &'a [NodeTarget],
) -> Vec<&'a NodeTarget> {
    let name = listener["proxy"].as_str().expect("监听器必须指定出口");
    let groups = config["proxy-groups"]
        .as_array()
        .expect("必须注册全局出口组");
    let group = groups
        .iter()
        .find(|g| g["name"] == name)
        .expect("监听器引用的出口必须存在");
    let filter = group["filter"].as_str().expect("必须限定单个节点");
    assert!(!filter.contains('`'), "mihomo 会按反引号拆开多个过滤器");
    let pattern = regex::Regex::new(filter).expect("字面量过滤器必须有效");
    nodes
        .iter()
        .filter(|target| {
            group["use"] == json!([format!("sub_{}", target.provider_id)])
                && pattern.is_match(&target.node)
        })
        .collect()
}

#[test]
fn listeners_resolve_provider_nodes_through_registered_groups() {
    let providers = [provider("one")];
    let nodes = [
        node("one", "香港节点", 19101),
        node("one", "美国节点", 19102),
    ];
    let cfg = config(&providers, &nodes);
    let listeners = cfg["listeners"].as_array().expect("listeners");
    assert_eq!(listeners.len(), nodes.len() + 1);
    for (listener, target) in listeners.iter().zip(&nodes) {
        let matched = matching_nodes(&cfg, listener, &nodes);
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].node, target.node);
        assert_ne!(listener["proxy"], target.node);
    }
}

#[test]
fn same_named_nodes_stay_scoped_to_their_own_subscription() {
    let providers = [provider("one"), provider("two")];
    let nodes = [
        node("one", "香港节点", 19101),
        node("two", "香港节点", 19102),
    ];
    let cfg = config(&providers, &nodes);
    let listeners = cfg["listeners"].as_array().expect("listeners");
    assert_ne!(listeners[0]["proxy"], listeners[1]["proxy"]);
    for (listener, target) in listeners.iter().zip(&nodes) {
        let matched = matching_nodes(&cfg, listener, &nodes);
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].provider_id, target.provider_id);
    }
}

#[test]
fn special_node_names_are_literal_and_match_the_entire_name() {
    for name in [
        "香港 [01] (专线)+ 2x?",
        "a.*|b$^",
        "node`other",
        "with\\path",
        "\\x60`\\\\`",
        "{节点}: #1 / 🚀",
        "line\nbreak",
        "trailing\n",
        "DIRECT",
        "GLOBAL",
    ] {
        let providers = [provider("one")];
        let nodes = [node("one", name, 19101)];
        let cfg = config(&providers, &nodes);
        let candidates = [
            node("one", name, 19101),
            node("one", &format!("{name} suffix"), 19102),
            node("one", &format!("prefix {name}"), 19103),
            node("one", &format!("{name}\n"), 19104),
            node("two", name, 19105),
        ];
        let matched = matching_nodes(&cfg, &cfg["listeners"][0], &candidates);
        assert_eq!(matched.len(), 1, "名称不应当作正则或跨订阅匹配: {name:?}");
        assert_eq!(matched[0].node, name);
    }
}

#[test]
fn disappeared_node_fails_closed_instead_of_using_another_exit() {
    let providers = [provider("one")];
    let nodes = [node("one", "gone", 19101)];
    let cfg = config(&providers, &nodes);
    let others = [node("one", "another", 19102), node("two", "gone", 19103)];
    assert!(matching_nodes(&cfg, &cfg["listeners"][0], &others).is_empty());
    for group in cfg["proxy-groups"].as_array().expect("groups").iter().filter(|group| group["name"] != TEST_GROUP_NAME) {
        assert_eq!(group["type"], "select");
        assert_eq!(group["empty-fallback"], "REJECT");
        assert!(group.get("proxies").is_none(), "不能附带其他出口作为候选");
        assert!(group.get("include-all").is_none());
        assert!(group.get("include-all-providers").is_none());
    }
}

#[test]
fn routing_fix_preserves_loopback_ports_and_subscription_settings() {
    let providers = [provider("one")];
    let nodes = [node("one", "node", 19107)];
    let cfg = config(&providers, &nodes);
    assert_eq!(cfg["listeners"][0]["port"], 19107);
    assert_eq!(cfg["listeners"][0]["listen"], "127.0.0.1");
    assert_eq!(cfg["listeners"][0]["type"], "mixed");
    assert_eq!(cfg["listeners"][0]["name"], "wbn_474cdf4fced7380f");
    assert_eq!(cfg["external-controller"], "127.0.0.1:19090");
    assert_eq!(cfg["secret"], "synthetic-controller-secret");
    assert_eq!(cfg["proxy-providers"]["sub_one"]["url"], providers[0].url);
    assert_eq!(
        cfg["proxy-providers"]["sub_one"]["path"],
        "providers/sub_one.yaml"
    );
    assert_eq!(cfg["rules"], json!(["MATCH,DIRECT"]));
    assert_eq!(cfg["profile"]["store-selected"], false);
    assert!(cfg.get("tun").is_none());
    assert!(cfg.get("allow-lan").is_none());
    assert_eq!(
        cfg,
        config(&providers, &nodes),
        "配置必须稳定，不改变节点端口或引用"
    );
}

#[test]
fn historical_ports_survive_reordered_nodes_and_subscription_names() {
    let ports = json!({"one::alpha": 19123, "one::beta": 19147, "two::alpha": 19162});
    let mut state = json!({"ports": ports});
    let targets = [node("two", "alpha", 19162), node("one", "beta", 19147)];
    // 纯端口注册表测试，不探测真实端口或访问内核。
    write_ports(&mut state, &targets);
    assert_eq!(state["ports"], ports);
    prune_ports(&mut state, &[provider("one")]);
    assert_eq!(state["ports"], json!({"one::alpha": 19123, "one::beta": 19147}));
}

#[test]
fn adjacent_backslashes_and_backticks_have_a_fixed_literal_encoding() {
    assert_eq!(
        exact_node_filter("\\x60`\\\\`"),
        r"\A(?:\\x60\x60\\\\\x60)\z"
    );
}

#[test]
fn bootstrap_without_cached_nodes_has_no_dangling_groups() {
    let cfg = config(&[provider("one")], &[]);
    assert_eq!(cfg["proxy-groups"], json!([]));
    assert_eq!(cfg["listeners"], json!([]));
    assert!(cfg["proxy-providers"]["sub_one"].is_object());
}

/// 本地配置文件走 `type: file`：直接读我们写的那份 YAML，**没有 interval**
/// —— 「本地配置不参与定时刷新」就落在这一点上（http provider 才有 interval
/// 与 health-check）。同时确认两者用的是同一个缓存路径：导入时写进去的文件
/// 就是内核读到的文件。
#[test]
fn unified_test_port_uses_exact_groups_without_allocating_node_ports() {
    let mut nodes = [node("one", "same`[name]", 19101), node("two", "same`[name]", 19102)];
    nodes[0].port = None;
    nodes[1].port = None;
    let cfg = config(&[file_provider("one"), provider("two")], &nodes);
    assert_eq!(cfg["listeners"].as_array().unwrap().len(), 1);
    assert_eq!(cfg["listeners"][0]["name"], TEST_LISTENER_NAME);
    assert_eq!(cfg["listeners"][0]["port"], 19080);
    let test = cfg["proxy-groups"].as_array().unwrap().iter()
        .find(|group| group["name"] == TEST_GROUP_NAME).unwrap();
    assert_eq!(test["proxies"].as_array().unwrap().len(), 2);
    for target in &nodes {
        let alias = group_of_key(&node_key(&target.provider_id, &target.node));
        assert!(test["proxies"].as_array().unwrap().contains(&json!(alias)));
        let matched = matching_nodes(&cfg, &json!({"proxy": alias}), &nodes);
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].provider_id, target.provider_id);
    }
    assert!(cfg.get("proxies").is_none(), "不能留下过期的内联节点定义");
}

#[test]
fn legacy_file_without_content_never_becomes_http() {
    let mut legacy = file_provider("legacy");
    legacy.kind = "file".to_string();
    legacy.content.clear();
    let cfg = config(&[legacy], &[]);
    assert_eq!(cfg["proxy-providers"]["sub_legacy"]["type"], "file");
    assert!(cfg["proxy-providers"]["sub_legacy"].get("url").is_none());
    assert!(cfg["proxy-providers"]["sub_legacy"].get("interval").is_none());
}

#[test]
fn local_file_provider_has_no_refresh_interval() {
    let cfg = config(&[provider("link"), file_provider("local")], &[]);
    let link = &cfg["proxy-providers"]["sub_link"];
    let local = &cfg["proxy-providers"]["sub_local"];
    assert_eq!(link["type"], json!("http"));
    assert_eq!(link["url"], json!("https://example.invalid/subscription"));
    assert_eq!(link["interval"], json!(86400));
    assert_eq!(local["type"], json!("file"));
    // 与 `subscriptions::provider_cache_path` 同一个值（导入时写盘的位置）：
    // id = `local` → `providers/sub_local.yaml`
    assert_eq!(local["path"], json!("providers/sub_local.yaml"));
    assert!(local.get("interval").is_none(), "本地文件不得有定时刷新");
    assert!(local.get("url").is_none());
}
