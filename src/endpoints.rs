//! Detect remote-interface "endpoint" types — WCF service contracts, ASP.NET
//! Web API controllers, SignalR hubs, .NET Remoting servers, and WCF client
//! proxies — and cross-index the consumer types that mention them.
//!
//! Inputs come entirely from `Project.type_metrics`: type-level `attributes`
//! and `bases` classify the endpoint kind, method-level `attributes` carry
//! the per-operation contract markers (`OperationContract`, `HttpGet`, …),
//! and per-type `referenced_types` lets us answer "who uses this contract?"
//! without re-parsing source.
//!
//! Projects with no endpoints are dropped from the snapshot to keep the
//! file small on monoliths.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::model::{MethodMetric, Project, TypeKind, TypeMetrics};

/// Kinds of remote interface we detect today. The string form lands in YAML
/// directly (`kind: wcf-contract`); keep it stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointKind {
    WcfContract,
    WcfClient,
    WebapiController,
    SignalrHub,
    Remoting,
}

impl EndpointKind {
    fn as_str(self) -> &'static str {
        match self {
            EndpointKind::WcfContract => "wcf-contract",
            EndpointKind::WcfClient => "wcf-client",
            EndpointKind::WebapiController => "webapi-controller",
            EndpointKind::SignalrHub => "signalr-hub",
            EndpointKind::Remoting => "remoting",
        }
    }
}

impl Serialize for EndpointKind {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

#[derive(Debug, Serialize)]
pub struct EndpointsSnapshot {
    pub projects: Vec<ProjectEndpoints>,
}

#[derive(Debug, Serialize)]
pub struct ProjectEndpoints {
    pub name: String,
    pub path: PathBuf,
    pub endpoints: Vec<Endpoint>,
}

#[derive(Debug, Serialize)]
pub struct Endpoint {
    pub kind: EndpointKind,
    /// Fully-qualified type name of the endpoint.
    #[serde(rename = "type")]
    pub type_fqn: String,
    /// Type-level route prefix (Web API only) — the value of an
    /// `[Route("...")]` on the controller class.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    /// Union of every reachable method's `dtos` — the data-shape contract
    /// surface across the whole endpoint. Sorted, deduped.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dtos: Vec<String>,
    /// Type-level attributes that aren't already consumed by classification
    /// (e.g. custom permission filters, `OverrideAuthorization`,
    /// `IgnoreCsrfFilter`). Names rendered exactly as captured (with args).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub extras: Vec<String>,
    /// Reachable methods. Plain names for methods with no aux info; mapping
    /// form when a verb/route/dtos/extras is present.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub methods: Vec<EndpointMethod>,
    /// Consumers of this contract, grouped by project. Each value is the
    /// sorted list of type local-names (simple names; namespace dropped to
    /// keep the file scannable) that mention this endpoint type.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub users: BTreeMap<String, Vec<String>>,
}

/// One reachable method on an endpoint. Renders as a bare YAML scalar when
/// only `name` is set; otherwise as a flow/block mapping with whichever
/// auxiliary fields are populated. `verb`/`route` apply to Web API only;
/// `dtos` and `extras` are general.
#[derive(Debug, Default)]
pub struct EndpointMethod {
    pub name: String,
    pub verb: Option<String>,
    pub route: Option<String>,
    /// Simple type names from the method's parameter list and return type
    /// (the data-shape contract — DataContract DTOs in WCF, request/response
    /// types in Web API). Sorted, deduped, primitives excluded.
    pub dtos: Vec<String>,
    /// Attributes applied to this method that aren't already consumed by
    /// classification (the verb attr, `Route`, etc.). Sorted, deduped.
    pub extras: Vec<String>,
}

impl Serialize for EndpointMethod {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let has_aux = self.verb.is_some()
            || self.route.is_some()
            || !self.dtos.is_empty()
            || !self.extras.is_empty();
        if !has_aux {
            return s.serialize_str(&self.name);
        }
        let mut count = 1;
        if self.verb.is_some() {
            count += 1;
        }
        if self.route.is_some() {
            count += 1;
        }
        if !self.dtos.is_empty() {
            count += 1;
        }
        if !self.extras.is_empty() {
            count += 1;
        }
        let mut m = s.serialize_map(Some(count))?;
        m.serialize_entry("name", &self.name)?;
        if let Some(v) = &self.verb {
            m.serialize_entry("verb", v)?;
        }
        if let Some(r) = &self.route {
            m.serialize_entry("route", r)?;
        }
        if !self.dtos.is_empty() {
            m.serialize_entry("dtos", &self.dtos)?;
        }
        if !self.extras.is_empty() {
            m.serialize_entry("extras", &self.extras)?;
        }
        m.end()
    }
}

pub fn build(projects: &[Project], scan_root: &Path) -> EndpointsSnapshot {
    let root = scan_root
        .canonicalize()
        .unwrap_or_else(|_| scan_root.to_path_buf());

    // Pass 1 — classify endpoints in every project.
    let mut per_project: Vec<ProjectEndpoints> = Vec::new();
    // Index: endpoint simple-name -> list of (project_idx_in_per_project,
    // endpoint_idx_within_that_project). Multiple endpoints can share a
    // simple name across projects; we credit users to all matches because
    // we don't have full FQN resolution at the reference site.
    let mut by_simple: BTreeMap<String, Vec<(usize, usize)>> = BTreeMap::new();

    for p in projects {
        let mut endpoints: Vec<Endpoint> = Vec::new();
        for (fqn, m) in &p.type_metrics {
            let kind_in_proj = p
                .declared_types
                .iter()
                .find(|(_, names)| names.iter().any(|n| n == fqn))
                .map(|(k, _)| *k);
            let Some(kind_in_proj) = kind_in_proj else {
                continue;
            };
            if let Some(ep) = classify(fqn, kind_in_proj, m) {
                endpoints.push(ep);
            }
        }
        if endpoints.is_empty() {
            continue;
        }
        endpoints.sort_by(|a, b| a.type_fqn.cmp(&b.type_fqn));
        let pe_idx = per_project.len();
        for (ep_idx, ep) in endpoints.iter().enumerate() {
            let simple = ep
                .type_fqn
                .rsplit('.')
                .next()
                .unwrap_or(&ep.type_fqn)
                .to_string();
            by_simple.entry(simple).or_default().push((pe_idx, ep_idx));
        }
        per_project.push(ProjectEndpoints {
            name: p.name.clone(),
            path: relativize(&p.path, &root),
            endpoints,
        });
    }

    // Pass 2 — cross-index consumers. Walk every type in every project; for
    // each `referenced_types` entry that names a known endpoint simple-name,
    // record the consumer under the matching endpoint(s).
    for p in projects {
        for (consumer_fqn, m) in &p.type_metrics {
            for r in &m.referenced_types {
                let Some(matches) = by_simple.get(r) else {
                    continue;
                };
                let consumer_simple = consumer_fqn
                    .rsplit('.')
                    .next()
                    .unwrap_or(consumer_fqn)
                    .to_string();
                for (pe_idx, ep_idx) in matches {
                    let ep = &mut per_project[*pe_idx].endpoints[*ep_idx];
                    // Skip the endpoint type referencing itself across the
                    // graph (uncommon, but cheap to guard).
                    if &ep.type_fqn == consumer_fqn {
                        continue;
                    }
                    ep.users
                        .entry(p.name.clone())
                        .or_default()
                        .push(consumer_simple.clone());
                }
            }
        }
    }

    // Dedup + sort each user list.
    for pe in &mut per_project {
        for ep in &mut pe.endpoints {
            for v in ep.users.values_mut() {
                v.sort();
                v.dedup();
            }
        }
    }

    per_project.sort_by(|a, b| a.name.cmp(&b.name));
    EndpointsSnapshot {
        projects: per_project,
    }
}

fn relativize(path: &Path, root: &Path) -> PathBuf {
    path.strip_prefix(root)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| path.to_path_buf())
}

fn classify(fqn: &str, kind: TypeKind, m: &TypeMetrics) -> Option<Endpoint> {
    if has_attr(&m.attributes, "ServiceContract") && kind == TypeKind::Interface {
        return Some(make_endpoint(
            fqn,
            EndpointKind::WcfContract,
            None,
            wcf_methods(m),
            m,
        ));
    }
    if has_base(&m.bases, "ClientBase") {
        return Some(make_endpoint(
            fqn,
            EndpointKind::WcfClient,
            None,
            Vec::new(),
            m,
        ));
    }
    let is_apicontroller_attr = has_attr(&m.attributes, "ApiController");
    // ASP.NET Core: ControllerBase/Controller. ASP.NET Web API on .NET
    // Framework: ApiController (System.Web.Http). All three are real.
    let is_controller_base = has_base(&m.bases, "ControllerBase")
        || has_base(&m.bases, "Controller")
        || has_base(&m.bases, "ApiController");
    if is_apicontroller_attr || (is_controller_base && any_http_method(m)) {
        let route = type_route(&m.attributes);
        return Some(make_endpoint(
            fqn,
            EndpointKind::WebapiController,
            route,
            http_methods(m),
            m,
        ));
    }
    if has_base(&m.bases, "Hub") {
        return Some(make_endpoint(
            fqn,
            EndpointKind::SignalrHub,
            None,
            hub_methods(m),
            m,
        ));
    }
    if has_base(&m.bases, "MarshalByRefObject") {
        return Some(make_endpoint(
            fqn,
            EndpointKind::Remoting,
            None,
            Vec::new(),
            m,
        ));
    }
    None
}

/// Attribute names that the classifier consumes directly. Anything else
/// applied to an endpoint type or method falls into the `extras:` bucket
/// for downstream auth/security analysis.
const CONSUMED_ATTRS: &[&str] = &[
    "ServiceContract",
    "OperationContract",
    "ApiController",
    "Route",
    "RoutePrefix",
    "HttpGet",
    "HttpPost",
    "HttpPut",
    "HttpDelete",
    "HttpPatch",
    "HttpHead",
    "HttpOptions",
];

fn extras_from(attrs: &[String]) -> Vec<String> {
    let mut out: Vec<String> = attrs
        .iter()
        .filter(|a| !CONSUMED_ATTRS.contains(&attr_name(a)))
        .cloned()
        .collect();
    out.sort();
    out.dedup();
    out
}

fn make_endpoint(
    fqn: &str,
    kind: EndpointKind,
    route: Option<String>,
    methods: Vec<EndpointMethod>,
    m: &TypeMetrics,
) -> Endpoint {
    // Endpoint-level dtos: union across method dtos, with the endpoint type
    // itself filtered out (a contract referencing its own simple name is
    // not a useful "DTO consumed by this endpoint" signal).
    let self_simple = fqn.rsplit('.').next().unwrap_or(fqn);
    let mut dtos: Vec<String> = methods
        .iter()
        .flat_map(|me| me.dtos.iter().cloned())
        .filter(|d| d != self_simple)
        .collect();
    dtos.sort();
    dtos.dedup();

    Endpoint {
        kind,
        type_fqn: fqn.to_string(),
        route,
        dtos,
        extras: extras_from(&m.attributes),
        methods,
        users: BTreeMap::new(),
    }
}

fn has_attr(attrs: &[String], name: &str) -> bool {
    attrs.iter().any(|a| attr_name(a) == name)
}

fn has_base(bases: &[String], name: &str) -> bool {
    bases.iter().any(|b| b == name)
}

/// Strip an optional `(...)` argument tail from a captured attribute string.
fn attr_name(s: &str) -> &str {
    s.split_once('(').map(|(n, _)| n).unwrap_or(s)
}

/// Pull the first positional string argument out of an attribute that was
/// rendered with our `'`-quoted convention. Returns `None` if the attribute
/// has no string argument.
fn first_string_arg(s: &str) -> Option<String> {
    let (_, rest) = s.split_once('(')?;
    let inside = rest.strip_suffix(')')?;
    let start = inside.find('\'')?;
    let after = &inside[start + 1..];
    let end = after.find('\'')?;
    Some(after[..end].to_string())
}

fn type_route(attrs: &[String]) -> Option<String> {
    // ASP.NET Core uses `[Route("...")]` on the controller; ASP.NET Web API
    // on .NET Framework uses `[RoutePrefix("...")]`. Both end up here.
    attrs
        .iter()
        .find(|a| matches!(attr_name(a), "Route" | "RoutePrefix"))
        .and_then(|a| first_string_arg(a))
}

fn wcf_methods(m: &TypeMetrics) -> Vec<EndpointMethod> {
    m.methods
        .iter()
        .filter(|meth| has_attr(&meth.attributes, "OperationContract"))
        .map(|meth| EndpointMethod {
            name: meth.name.clone(),
            verb: None,
            route: None,
            dtos: meth.signature_types.clone(),
            extras: extras_from(&meth.attributes),
        })
        .collect()
}

fn hub_methods(m: &TypeMetrics) -> Vec<EndpointMethod> {
    // Hub methods aren't attribute-marked. We don't track method visibility,
    // so we list every method-shaped member and let the consumer filter.
    m.methods
        .iter()
        .map(|meth| EndpointMethod {
            name: meth.name.clone(),
            verb: None,
            route: None,
            dtos: meth.signature_types.clone(),
            extras: extras_from(&meth.attributes),
        })
        .collect()
}

const HTTP_VERB_ATTRS: &[(&str, &str)] = &[
    ("HttpGet", "GET"),
    ("HttpPost", "POST"),
    ("HttpPut", "PUT"),
    ("HttpDelete", "DELETE"),
    ("HttpPatch", "PATCH"),
    ("HttpHead", "HEAD"),
    ("HttpOptions", "OPTIONS"),
];

fn any_http_method(m: &TypeMetrics) -> bool {
    m.methods.iter().any(|meth| {
        meth.attributes
            .iter()
            .any(|a| HTTP_VERB_ATTRS.iter().any(|(n, _)| attr_name(a) == *n))
    })
}

fn http_methods(m: &TypeMetrics) -> Vec<EndpointMethod> {
    m.methods.iter().filter_map(http_method_for).collect()
}

fn http_method_for(meth: &MethodMetric) -> Option<EndpointMethod> {
    // First Http* match wins. The route may be either inside the verb attr
    // (`[HttpGet("{id}")]`, ASP.NET Core style) or on a separate sibling
    // `[Route("...")]` (Web API on .NET Framework style). Inline arg wins
    // when both are present.
    let mut verb: Option<&'static str> = None;
    let mut inline_route: Option<String> = None;
    for attr in &meth.attributes {
        if let Some((_, v)) = HTTP_VERB_ATTRS
            .iter()
            .find(|(n, _)| attr_name(attr) == *n)
        {
            verb = Some(*v);
            inline_route = first_string_arg(attr);
            break;
        }
    }
    let verb = verb?;
    let route = inline_route.or_else(|| {
        meth.attributes
            .iter()
            .find(|a| attr_name(a) == "Route")
            .and_then(|a| first_string_arg(a))
    });
    Some(EndpointMethod {
        name: meth.name.clone(),
        verb: Some(verb.to_string()),
        route,
        dtos: meth.signature_types.clone(),
        extras: extras_from(&meth.attributes),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MethodMetric, ProjectId, SourceSpan};

    fn mk_method(name: &str, attrs: &[&str]) -> MethodMetric {
        mk_method_full(name, attrs, &[])
    }

    fn mk_method_full(name: &str, attrs: &[&str], dtos: &[&str]) -> MethodMetric {
        MethodMetric {
            name: name.to_string(),
            line_start: 1,
            line_end: 2,
            loc: 2,
            complexity: 0,
            file_id: None,
            attributes: attrs.iter().map(|s| s.to_string()).collect(),
            signature_types: dtos.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn mk_type(attrs: &[&str], bases: &[&str], methods: Vec<MethodMetric>) -> TypeMetrics {
        TypeMetrics {
            loc: 10,
            members: methods.len() as u32,
            complexity: 0,
            spans: vec![SourceSpan {
                file_id: 0,
                line_start: 1,
                line_end: 10,
            }],
            methods,
            bases: bases.iter().map(|s| s.to_string()).collect(),
            attributes: attrs.iter().map(|s| s.to_string()).collect(),
            referenced_types: Vec::new(),
        }
    }

    fn mk_project(name: &str, types: Vec<(TypeKind, &str, TypeMetrics)>) -> Project {
        let mut declared_types: BTreeMap<TypeKind, Vec<String>> = BTreeMap::new();
        let mut type_metrics: BTreeMap<String, TypeMetrics> = BTreeMap::new();
        for (k, fqn, m) in types {
            declared_types
                .entry(k)
                .or_default()
                .push(fqn.to_string());
            type_metrics.insert(fqn.to_string(), m);
        }
        Project {
            id: ProjectId::from_path(Path::new(name)),
            path: PathBuf::from(format!("/r/{name}.csproj")),
            name: name.to_string(),
            sdk_style: true,
            target_frameworks: vec![],
            package_refs: vec![],
            project_refs: vec![],
            assembly_refs: vec![],
            usings: vec![],
            declared_namespaces: vec![],
            declared_types,
            type_metrics,
            referenced_types: Vec::new(),
            source_files: Vec::new(),
        }
    }

    #[test]
    fn classifies_wcf_contract_with_operation_contracts() {
        let iface = mk_type(
            &["ServiceContract"],
            &[],
            vec![
                mk_method("Submit", &["OperationContract"]),
                mk_method("Cancel", &["OperationContract"]),
                mk_method("HiddenHelper", &[]),
            ],
        );
        let p = mk_project(
            "Acme.Billing",
            vec![(TypeKind::Interface, "Acme.Billing.IInvoiceService", iface)],
        );
        let snap = build(&[p], Path::new("/r"));
        assert_eq!(snap.projects.len(), 1);
        assert_eq!(snap.projects[0].endpoints.len(), 1);
        let ep = &snap.projects[0].endpoints[0];
        assert_eq!(ep.kind, EndpointKind::WcfContract);
        assert_eq!(ep.methods.len(), 2);
        assert_eq!(ep.methods[0].name, "Submit");
        // No verb/route/dtos/extras on a bare WCF method — renders as scalar.
        assert!(ep.methods[0].verb.is_none());
        assert!(ep.methods[0].dtos.is_empty());
    }

    #[test]
    fn surfaces_dtos_per_method_and_at_endpoint_level() {
        let iface = mk_type(
            &["ServiceContract"],
            &[],
            vec![
                mk_method_full(
                    "Submit",
                    &["OperationContract"],
                    &["InvoiceDto", "OperationResult"],
                ),
                mk_method_full(
                    "Cancel",
                    &["OperationContract"],
                    &["CancelRequestDto", "OperationResult"],
                ),
            ],
        );
        let p = mk_project(
            "Acme.Billing",
            vec![(TypeKind::Interface, "Acme.Billing.IInvoiceService", iface)],
        );
        let snap = build(&[p], Path::new("/r"));
        let ep = &snap.projects[0].endpoints[0];
        assert_eq!(
            ep.dtos,
            vec![
                "CancelRequestDto".to_string(),
                "InvoiceDto".to_string(),
                "OperationResult".to_string(),
            ]
        );
        assert_eq!(
            ep.methods[0].dtos,
            vec!["InvoiceDto".to_string(), "OperationResult".to_string()]
        );
    }

    #[test]
    fn captures_extras_from_unrecognised_attributes() {
        // Real-world p2p Edge controller pattern: custom permission filter,
        // CSRF override, etc. None of these are consumed by classification,
        // so they should land in `extras:`.
        let ctrl = mk_type(
            &[
                "RoutePrefix('cat/api/perms')",
                "OverrideAuthorization",
                "IgnoreCsrfFilter",
            ],
            &["ApiController"],
            vec![mk_method(
                "Approve",
                &["HttpPost", "Route('approve')", "InvoicePermissionFilter"],
            )],
        );
        let p = mk_project(
            "DM.CAT.Api",
            vec![(TypeKind::Class, "Foo.PermsController", ctrl)],
        );
        let snap = build(&[p], Path::new("/r"));
        let ep = &snap.projects[0].endpoints[0];
        assert_eq!(
            ep.extras,
            vec!["IgnoreCsrfFilter".to_string(), "OverrideAuthorization".to_string()]
        );
        assert_eq!(
            ep.methods[0].extras,
            vec!["InvoicePermissionFilter".to_string()]
        );
    }

    #[test]
    fn classifies_webapi_controller_with_routes() {
        let ctrl = mk_type(
            &["ApiController", "Route('api/invoices')"],
            &["ControllerBase"],
            vec![
                mk_method("List", &["HttpGet"]),
                mk_method("GetOne", &["HttpGet('{id}')"]),
                mk_method("Post", &["HttpPost"]),
            ],
        );
        let p = mk_project(
            "Acme.Web",
            vec![(
                TypeKind::Class,
                "Acme.Web.InvoicesController",
                ctrl,
            )],
        );
        let snap = build(&[p], Path::new("/r"));
        let ep = &snap.projects[0].endpoints[0];
        assert_eq!(ep.kind, EndpointKind::WebapiController);
        assert_eq!(ep.route.as_deref(), Some("api/invoices"));
        assert_eq!(ep.methods.len(), 3);
        assert_eq!(ep.methods[1].name, "GetOne");
        assert_eq!(ep.methods[1].verb.as_deref(), Some("GET"));
        assert_eq!(ep.methods[1].route.as_deref(), Some("{id}"));
    }

    #[test]
    fn classifies_dotnet_framework_webapi_with_route_attr() {
        // .NET Framework Web API: base ApiController, type-level RoutePrefix,
        // method-level Route alongside Http* (instead of inline).
        let ctrl = mk_type(
            &["RoutePrefix('cat/api/catreporting')"],
            &["ApiController"],
            vec![mk_method(
                "GetReports",
                &["HttpGet", "Route('getreports')"],
            )],
        );
        let p = mk_project(
            "DM.CAT.Api",
            vec![(
                TypeKind::Class,
                "Basware.P2P.Edge.DM.CAT.Api.Controllers.ComplianceReportListController",
                ctrl,
            )],
        );
        let snap = build(&[p], Path::new("/r"));
        let ep = &snap.projects[0].endpoints[0];
        assert_eq!(ep.kind, EndpointKind::WebapiController);
        assert_eq!(ep.route.as_deref(), Some("cat/api/catreporting"));
        assert_eq!(ep.methods[0].name, "GetReports");
        assert_eq!(ep.methods[0].verb.as_deref(), Some("GET"));
        assert_eq!(ep.methods[0].route.as_deref(), Some("getreports"));
    }

    #[test]
    fn cross_indexes_consumers_by_project() {
        let svc = mk_type(&["ServiceContract"], &[], vec![]);
        let provider = mk_project(
            "Acme.Billing",
            vec![(TypeKind::Interface, "Acme.Billing.IInvoiceService", svc)],
        );

        let mut consumer_a = mk_type(&[], &[], vec![]);
        consumer_a.referenced_types = vec!["IInvoiceService".to_string()];
        let mut consumer_b = mk_type(&[], &[], vec![]);
        consumer_b.referenced_types = vec!["IInvoiceService".to_string()];
        let web = mk_project(
            "Acme.Web",
            vec![
                (TypeKind::Class, "Acme.Web.InvoicesController", consumer_a),
                (TypeKind::Class, "Acme.Web.AdminController", consumer_b),
            ],
        );

        let mut worker_t = mk_type(&[], &[], vec![]);
        worker_t.referenced_types = vec!["IInvoiceService".to_string()];
        let worker = mk_project(
            "Acme.Worker",
            vec![(TypeKind::Class, "Acme.Worker.InvoiceProcessor", worker_t)],
        );

        let snap = build(&[provider, web, worker], Path::new("/r"));
        let ep = &snap
            .projects
            .iter()
            .find(|p| p.name == "Acme.Billing")
            .unwrap()
            .endpoints[0];
        assert_eq!(
            ep.users.get("Acme.Web").unwrap(),
            &vec!["AdminController".to_string(), "InvoicesController".to_string()]
        );
        assert_eq!(
            ep.users.get("Acme.Worker").unwrap(),
            &vec!["InvoiceProcessor".to_string()]
        );
    }

    #[test]
    fn filters_self_reference() {
        let mut svc = mk_type(&["ServiceContract"], &[], vec![]);
        svc.referenced_types = vec!["IInvoiceService".to_string()];
        let p = mk_project(
            "Acme.Billing",
            vec![(TypeKind::Interface, "Acme.Billing.IInvoiceService", svc)],
        );
        let snap = build(&[p], Path::new("/r"));
        assert!(snap.projects[0].endpoints[0].users.is_empty());
    }
}

