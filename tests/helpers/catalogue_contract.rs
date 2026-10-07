use serde_json::Value;

// Validate schema positions independently of wire compaction. Literal defaults,
// examples, constraints and properties named description retain their meaning.
fn assert_wire_tool_contract(wire: &Value, full: &Value) {
    fn schema(wire: &Value, full: &Value) {
        let Some(expected) = full.as_object() else {
            assert_eq!(wire, full);
            return;
        };
        let actual = wire.as_object().expect("schema object");
        assert_eq!(
            actual.len(),
            expected.len()
                - usize::from(
                    expected.contains_key("description") && !actual.contains_key("description")
                )
        );
        for (key, value) in expected {
            if key == "description" {
                if let Some(got) = actual.get(key) {
                    let text = got.as_str().expect("description string");
                    assert!(text.len() <= 100);
                    assert!(!text.is_empty());
                }
                continue;
            }
            let got = actual
                .get(key)
                .unwrap_or_else(|| panic!("schema constraint removed: {key}"));
            match key.as_str() {
                "properties" | "patternProperties" | "$defs" | "definitions"
                | "dependentSchemas" => {
                    let names = value.as_object().unwrap();
                    let observed = got.as_object().unwrap();
                    assert_eq!(observed.len(), names.len());
                    for (name, child) in names {
                        schema(
                            observed.get(name).expect("property/definition retained"),
                            child,
                        );
                    }
                }
                "allOf" | "anyOf" | "oneOf" | "prefixItems" => {
                    let items = value.as_array().unwrap();
                    let observed = got.as_array().unwrap();
                    assert_eq!(items.len(), observed.len());
                    for (child, expected) in observed.iter().zip(items) {
                        schema(child, expected);
                    }
                }
                "items"
                | "additionalProperties"
                | "unevaluatedProperties"
                | "additionalItems"
                | "not"
                | "if"
                | "then"
                | "else"
                | "contains"
                | "propertyNames"
                | "unevaluatedItems" => {
                    if let Some(items) = value.as_array() {
                        let observed = got.as_array().unwrap();
                        assert_eq!(observed.len(), items.len());
                        for (child, expected) in observed.iter().zip(items) {
                            schema(child, expected);
                        }
                    } else {
                        schema(got, value);
                    }
                }
                "dependencies" => {
                    let expected = value.as_object().unwrap();
                    let actual = got.as_object().unwrap();
                    assert_eq!(actual.len(), expected.len());
                    for (name, child) in expected {
                        let observed = actual.get(name).expect("dependency retained");
                        if child.is_object() {
                            schema(observed, child);
                        } else {
                            assert_eq!(observed, child);
                        }
                    }
                }
                _ => assert_eq!(got, value, "literal/constraint fidelity: {key}"),
            }
        }
    }
    assert_eq!(
        wire.as_object().unwrap().len(),
        full.as_object().unwrap().len()
    );
    for (name, parameter) in full["inputSchema"]["properties"].as_object().unwrap() {
        let authored = parameter["description"]
            .as_str()
            .expect("authored parameter guidance");
        assert!(!authored.is_empty(), "missing source guidance: {name}");
        let caption = wire["inputSchema"]["properties"][name]["description"]
            .as_str()
            .expect("wire parameter guidance retained");
        assert!(
            !caption.trim().is_empty() && caption.len() <= 100,
            "invalid guidance: {name}"
        );
    }
    for (key, value) in full.as_object().unwrap() {
        match key.as_str() {
            "inputSchema" => schema(&wire[key], value),
            "description" => assert!(wire[key].as_str().unwrap().len() <= 200),
            _ => assert_eq!(&wire[key], value),
        }
    }
}

pub fn assert_complete_catalogue_page(page: &Value, expected: &[Value]) {
    assert!(
        nestweaver_mcp::output_budget::escaped_size(page)
            <= nestweaver_mcp::output_budget::CATALOGUE_BYTES
    );
    assert!(
        page.get("nextCursor").is_none(),
        "first-page discovery must be complete"
    );
    let tools = page["tools"].as_array().expect("catalogue tool array");
    assert!(!tools.is_empty());
    assert_eq!(tools.len(), expected.len(), "complete visible catalogue");
    let mut names = std::collections::HashSet::new();
    for (wire, full) in tools.iter().zip(expected) {
        assert!(
            names.insert(wire["name"].as_str().unwrap()),
            "duplicate tool: {wire}"
        );
        assert_wire_tool_contract(wire, full);
    }
}
