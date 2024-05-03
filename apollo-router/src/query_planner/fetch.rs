use std::collections::HashMap;
use std::fmt::Display;
use std::sync::Arc;
use std::sync::OnceLock;

use apollo_compiler::validation::Valid;
use apollo_compiler::ExecutableDocument;
use apollo_compiler::NodeStr;
use indexmap::IndexSet;
use router_bridge::planner::PlanSuccess;
use router_bridge::planner::Planner;
use serde::Deserialize;
use serde::Serialize;
use tower::ServiceExt;
use tracing::instrument;
use tracing::Instrument;

use super::execution::ExecutionParameters;
use super::rewrites;
use super::selection::execute_selection_set;
use super::selection::Selection;
use super::PlanNode;
use super::QueryPlanResult;
use crate::error::Error;
use crate::error::FetchError;
use crate::error::QueryPlannerError;
use crate::graphql;
use crate::graphql::Request;
use crate::http_ext;
use crate::json_ext;
use crate::json_ext::Object;
use crate::json_ext::Path;
use crate::json_ext::Value;
use crate::json_ext::ValueExt;
use crate::plugins::authorization::AuthorizationPlugin;
use crate::plugins::authorization::CacheKeyMetadata;
use crate::plugins::connectors::finder_field_for_fetch_node;
use crate::plugins::connectors::Connector;
use crate::services::SubgraphRequest;
use crate::spec::query::change::QueryHashVisitor;
use crate::spec::Schema;

/// GraphQL operation type.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum OperationKind {
    #[default]
    Query,
    Mutation,
    Subscription,
}

impl Display for OperationKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.default_type_name())
    }
}

impl OperationKind {
    pub(crate) const fn default_type_name(&self) -> &'static str {
        match self {
            OperationKind::Query => "Query",
            OperationKind::Mutation => "Mutation",
            OperationKind::Subscription => "Subscription",
        }
    }

    /// Only for apollo studio exporter
    pub(crate) const fn as_apollo_operation_type(&self) -> &'static str {
        match self {
            OperationKind::Query => "query",
            OperationKind::Mutation => "mutation",
            OperationKind::Subscription => "subscription",
        }
    }
}

impl From<OperationKind> for apollo_compiler::ast::OperationType {
    fn from(value: OperationKind) -> Self {
        match value {
            OperationKind::Query => apollo_compiler::ast::OperationType::Query,
            OperationKind::Mutation => apollo_compiler::ast::OperationType::Mutation,
            OperationKind::Subscription => apollo_compiler::ast::OperationType::Subscription,
        }
    }
}

impl From<apollo_compiler::ast::OperationType> for OperationKind {
    fn from(value: apollo_compiler::ast::OperationType) -> Self {
        match value {
            apollo_compiler::ast::OperationType::Query => OperationKind::Query,
            apollo_compiler::ast::OperationType::Mutation => OperationKind::Mutation,
            apollo_compiler::ast::OperationType::Subscription => OperationKind::Subscription,
        }
    }
}

pub(crate) type SubgraphSchemas = HashMap<String, Arc<Valid<apollo_compiler::Schema>>>;

/// A fetch node.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FetchNode {
    /// The name of the service or subgraph that the fetch is querying.
    pub(crate) service_name: NodeStr,

    /// The data that is required for the subgraph fetch.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[serde(default)]
    pub(crate) requires: Vec<Selection>,

    /// The variables that are used for the subgraph fetch.
    pub(crate) variable_usages: Vec<NodeStr>,

    /// The GraphQL subquery that is used for the fetch.
    pub(crate) operation: SubgraphOperation,

    /// The GraphQL subquery operation name.
    pub(crate) operation_name: Option<NodeStr>,

    /// The GraphQL operation kind that is used for the fetch.
    pub(crate) operation_kind: OperationKind,

    /// Optional id used by Deferred nodes
    pub(crate) id: Option<NodeStr>,

    // Optionally describes a number of "rewrites" that query plan executors should apply to the data that is sent as input of this fetch.
    pub(crate) input_rewrites: Option<Vec<rewrites::DataRewrite>>,

    // Optionally describes a number of "rewrites" to apply to the data that received from a fetch (and before it is applied to the current in-memory results).
    pub(crate) output_rewrites: Option<Vec<rewrites::DataRewrite>>,

    // hash for the query and relevant parts of the schema. if two different schemas provide the exact same types, fields and directives
    // affecting the query, then they will have the same hash
    #[serde(default)]
    pub(crate) schema_aware_hash: Arc<QueryHash>,

    // authorization metadata for the subgraph query
    #[serde(default)]
    pub(crate) authorization: Arc<CacheKeyMetadata>,
    #[serde(default)]
    pub(crate) protocol: Arc<Protocol>,
}

#[derive(Debug, Default, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Protocol {
    #[default]
    GraphQL,
    RestWrapper(RestProtocolWrapper),
    RestFetch(RestFetchNode),
}

#[derive(Debug, Default, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RestProtocolWrapper {
    pub(crate) connector_service_name: String,
    pub(crate) magic_finder_field: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub(crate) struct RestFetchNode {
    connector_service_name: String,
    parent_service_name: String,
}

#[derive(Clone)]
pub(crate) struct SubgraphOperation {
    // At least one of these two must be initialized
    serialized: OnceLock<String>,
    parsed: OnceLock<Arc<Valid<ExecutableDocument>>>,
}

impl SubgraphOperation {
    pub(crate) fn replace(&self, from: &str, to: &str) -> Self {
        let serialized = self
            .serialized
            .get()
            .map(|operation| operation.replace(from, to));

        Self::from_string(serialized.unwrap_or_default())
    }

    pub(crate) fn from_string(serialized: impl Into<String>) -> Self {
        Self {
            serialized: OnceLock::from(serialized.into()),
            parsed: OnceLock::new(),
        }
    }

    pub(crate) fn from_parsed(parsed: impl Into<Arc<Valid<ExecutableDocument>>>) -> Self {
        Self {
            serialized: OnceLock::new(),
            parsed: OnceLock::from(parsed.into()),
        }
    }

    pub(crate) fn as_serialized(&self) -> &str {
        self.serialized.get_or_init(|| {
            self.parsed
                .get()
                .expect("SubgraphOperation has neither representation initialized")
                .to_string()
        })
    }

    pub(crate) fn as_parsed(
        &self,
        subgraph_schema: &Valid<apollo_compiler::Schema>,
    ) -> &Arc<Valid<ExecutableDocument>> {
        self.parsed.get_or_init(|| {
            let serialized = self
                .serialized
                .get()
                .expect("SubgraphOperation has neither representation initialized");

            println!(
                "\n -----------------\n {} \n -----------------",
                subgraph_schema
            );
            Arc::new(
                ExecutableDocument::parse_and_validate(
                    subgraph_schema,
                    serialized,
                    "operation.graphql",
                )
                .map_err(|e| e.errors)
                .expect("Subgraph operation should be valid"),
            )
        })
    }
}

impl Serialize for SubgraphOperation {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.as_serialized().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SubgraphOperation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(Self::from_string(String::deserialize(deserializer)?))
    }
}

impl PartialEq for SubgraphOperation {
    fn eq(&self, other: &Self) -> bool {
        self.as_serialized() == other.as_serialized()
    }
}

impl std::fmt::Debug for SubgraphOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.as_serialized(), f)
    }
}

impl std::fmt::Display for SubgraphOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self.as_serialized(), f)
    }
}

#[derive(Clone, Default, Hash, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct QueryHash(#[serde(with = "hex")] pub(crate) Vec<u8>);

impl std::fmt::Debug for QueryHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("QueryHash")
            .field(&hex::encode(&self.0))
            .finish()
    }
}

impl Display for QueryHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", hex::encode(&self.0))
    }
}

pub(crate) struct Variables {
    pub(crate) variables: Object,
    pub(crate) inverted_paths: Vec<Vec<Path>>,
}

impl Variables {
    #[instrument(skip_all, level = "debug", name = "make_variables")]
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        requires: &[Selection],
        variable_usages: &[NodeStr],
        data: &Value,
        current_dir: &Path,
        request: &Arc<http::Request<Request>>,
        schema: &Schema,
        input_rewrites: &Option<Vec<rewrites::DataRewrite>>,
    ) -> Option<Variables> {
        let body = request.body();
        if !requires.is_empty() {
            let mut variables = Object::with_capacity(1 + variable_usages.len());

            variables.extend(variable_usages.iter().filter_map(|key| {
                body.variables
                    .get_key_value(key.as_str())
                    .map(|(variable_key, value)| (variable_key.clone(), value.clone()))
            }));

            let mut inverted_paths: Vec<Vec<Path>> = Vec::new();
            let mut values: IndexSet<Value> = IndexSet::new();

            data.select_values_and_paths(schema, current_dir, |path, value| {
                let mut value = execute_selection_set(value, requires, schema, None);
                if value.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
                    rewrites::apply_rewrites(schema, &mut value, input_rewrites);
                    match values.get_index_of(&value) {
                        Some(index) => {
                            inverted_paths[index].push(path.clone());
                        }
                        None => {
                            inverted_paths.push(vec![path.clone()]);
                            values.insert(value);
                            debug_assert!(inverted_paths.len() == values.len());
                        }
                    }
                }
            });

            if values.is_empty() {
                return None;
            }

            let representations = Value::Array(Vec::from_iter(values));

            variables.insert("representations", representations);
            Some(Variables {
                variables,
                inverted_paths,
            })
        } else {
            // with nested operations (Query or Mutation has an operation returning a Query or Mutation),
            // when the first fetch fails, the query plan will still execute up until the second fetch,
            // where `requires` is empty (not a federated fetch), the current dir is not emmpty (child of
            // the previous operation field) and the data is null. In that case, we recognize that we
            // should not perform the next fetch
            if !current_dir.is_empty()
                && data
                    .get_path(schema, current_dir)
                    .map(|value| value.is_null())
                    .unwrap_or(true)
            {
                return None;
            }

            Some(Variables {
                variables: variable_usages
                    .iter()
                    .filter_map(|key| {
                        body.variables
                            .get_key_value(key.as_str())
                            .map(|(variable_key, value)| (variable_key.clone(), value.clone()))
                    })
                    .collect::<Object>(),
                inverted_paths: Vec::new(),
            })
        }
    }
}

impl FetchNode {
    pub(crate) fn parsed_operation(
        &self,
        subgraph_schemas: &SubgraphSchemas,
    ) -> &Arc<Valid<ExecutableDocument>> {
        self.operation
            .as_parsed(&subgraph_schemas[self.service_name().as_str()])
    }

    pub(crate) fn service_name(&self) -> NodeStr {
        match self.protocol.as_ref() {
            Protocol::GraphQL => self.service_name.clone(),
            Protocol::RestWrapper(rw) => rw.connector_service_name.clone().into(),
            Protocol::RestFetch(rf) => rf.connector_service_name.clone().into(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn fetch_node<'a>(
        &'a self,
        parameters: &'a ExecutionParameters<'a>,
        data: &'a Value,
        current_dir: &'a Path,
    ) -> (Value, Vec<Error>) {
        let FetchNode {
            operation,
            operation_kind,
            operation_name,
            service_name,
            ..
        } = self;

        let Variables {
            variables,
            inverted_paths: paths,
        } = match Variables::new(
            &self.requires,
            &self.variable_usages,
            data,
            current_dir,
            // Needs the original request here
            parameters.supergraph_request,
            parameters.schema,
            &self.input_rewrites,
        ) {
            Some(variables) => variables,
            None => {
                return (Value::Object(Object::default()), Vec::new());
            }
        };

        let service_name_string = service_name.to_string();

        let (service_name, subgraph_service_name) = match &*self.protocol {
            Protocol::RestFetch(RestFetchNode {
                connector_service_name,
                parent_service_name,
            }) => (parent_service_name, connector_service_name),
            _ => (&service_name_string, &service_name_string),
        };

        let uri = parameters
            .schema
            .subgraph_url(service_name)
            .unwrap_or_else(|| {
                panic!("schema uri for subgraph '{service_name}' should already have been checked")
            })
            .clone();

        let mut subgraph_request = SubgraphRequest::builder()
            .supergraph_request(parameters.supergraph_request.clone())
            .subgraph_request(
                http_ext::Request::builder()
                    .method(http::Method::POST)
                    .uri(uri)
                    .body(
                        Request::builder()
                            .query(operation.as_serialized())
                            .and_operation_name(operation_name.as_ref().map(|n| n.to_string()))
                            .variables(variables.clone())
                            .build(),
                    )
                    .build()
                    .expect("it won't fail because the url is correct and already checked; qed"),
            )
            .subgraph_name(subgraph_service_name)
            .operation_kind(*operation_kind)
            .context(parameters.context.clone())
            .build();
        subgraph_request.query_hash = self.schema_aware_hash.clone();
        subgraph_request.authorization = self.authorization.clone();

        let service = parameters
            .service_factory
            .create(service_name)
            .expect("we already checked that the service exists during planning; qed");

        let (_parts, response) = match service
            .oneshot(subgraph_request)
            .instrument(tracing::trace_span!("subfetch_stream"))
            .await
            // TODO this is a problem since it restores details about failed service
            // when errors have been redacted in the include_subgraph_errors module.
            // Unfortunately, not easy to fix here, because at this point we don't
            // know if we should be redacting errors for this subgraph...
            .map_err(|e| match e.downcast::<FetchError>() {
                Ok(inner) => match *inner {
                    FetchError::SubrequestHttpError { .. } => *inner,
                    _ => FetchError::SubrequestHttpError {
                        status_code: None,
                        service: service_name.to_string(),
                        reason: inner.to_string(),
                    },
                },
                Err(e) => FetchError::SubrequestHttpError {
                    status_code: None,
                    service: service_name.to_string(),
                    reason: e.to_string(),
                },
            }) {
            Err(e) => {
                return (
                    Value::default(),
                    vec![e.to_graphql_error(Some(current_dir.to_owned()))],
                );
            }
            Ok(res) => res.response.into_parts(),
        };

        super::log::trace_subfetch(
            service_name,
            operation.as_serialized(),
            &variables,
            &response,
        );

        if !response.is_primary() {
            return (
                Value::default(),
                vec![FetchError::SubrequestUnexpectedPatchResponse {
                    service: service_name.to_string(),
                }
                .to_graphql_error(Some(current_dir.to_owned()))],
            );
        }

        let (value, errors) =
            self.response_at_path(parameters.schema, current_dir, paths, response);
        if let Some(id) = &self.id {
            if let Some(sender) = parameters.deferred_fetches.get(id.as_str()) {
                tracing::info!(monotonic_counter.apollo.router.operations.defer.fetch = 1u64);
                if let Err(e) = sender.clone().send((value.clone(), errors.clone())) {
                    tracing::error!("error sending fetch result at path {} and id {:?} for deferred response building: {}", current_dir, self.id, e);
                }
            }
        }
        (value, errors)
    }

    #[instrument(skip_all, level = "debug", name = "response_insert")]
    fn response_at_path<'a>(
        &'a self,
        schema: &Schema,
        current_dir: &'a Path,
        inverted_paths: Vec<Vec<Path>>,
        response: graphql::Response,
    ) -> (Value, Vec<Error>) {
        if !self.requires.is_empty() {
            let entities_path = Path(vec![json_ext::PathElement::Key(
                "_entities".to_string(),
                None,
            )]);

            let mut errors: Vec<Error> = vec![];
            for mut error in response.errors {
                // the locations correspond to the subgraph query and cannot be linked to locations
                // in the client query, so we remove them
                error.locations = Vec::new();

                // errors with path should be updated to the path of the entity they target
                if let Some(ref path) = error.path {
                    if path.starts_with(&entities_path) {
                        // the error's path has the format '/_entities/1/other' so we ignore the
                        // first element and then get the index
                        match path.0.get(1) {
                            Some(json_ext::PathElement::Index(i)) => {
                                for values_path in
                                    inverted_paths.get(*i).iter().flat_map(|v| v.iter())
                                {
                                    errors.push(Error {
                                        locations: error.locations.clone(),
                                        // append to the entitiy's path the error's path without
                                        //`_entities` and the index
                                        path: Some(Path::from_iter(
                                            values_path.0.iter().chain(&path.0[2..]).cloned(),
                                        )),
                                        message: error.message.clone(),
                                        extensions: error.extensions.clone(),
                                    })
                                }
                            }
                            _ => {
                                error.path = Some(current_dir.clone());
                                errors.push(error)
                            }
                        }
                    } else {
                        error.path = Some(current_dir.clone());
                        errors.push(error);
                    }
                } else {
                    errors.push(error);
                }
            }

            // we have to nest conditions and do early returns here
            // because we need to take ownership of the inner value
            if let Some(Value::Object(mut map)) = response.data {
                if let Some(entities) = map.remove("_entities") {
                    tracing::trace!("received entities: {:?}", &entities);

                    if let Value::Array(array) = entities {
                        let mut value = Value::default();

                        for (index, mut entity) in array.into_iter().enumerate() {
                            rewrites::apply_rewrites(schema, &mut entity, &self.output_rewrites);

                            if let Some(paths) = inverted_paths.get(index) {
                                if paths.len() > 1 {
                                    for path in &paths[1..] {
                                        let _ = value.insert(path, entity.clone());
                                    }
                                }

                                if let Some(path) = paths.first() {
                                    let _ = value.insert(path, entity);
                                }
                            }
                        }
                        return (value, errors);
                    }
                }
            }

            // if we get here, it means that the response was missing the `_entities` key
            // This can happen if the subgraph failed during query execution e.g. for permissions checks.
            // In this case we should add an additional error because the subgraph should have returned an error that will be bubbled up to the client.
            // However, if they have not then print a warning to the logs.
            if errors.is_empty() {
                tracing::warn!(
                    "Subgraph response from '{}' was missing key `_entities` and had no errors. This is likely a bug in the subgraph.",
                    self.service_name
                );
            }

            (Value::Null, errors)
        } else {
            let current_slice =
                if matches!(current_dir.last(), Some(&json_ext::PathElement::Flatten(_))) {
                    &current_dir.0[..current_dir.0.len() - 1]
                } else {
                    &current_dir.0[..]
                };

            let errors: Vec<Error> = response
                .errors
                .into_iter()
                .map(|error| {
                    let path = error.path.as_ref().map(|path| {
                        Path::from_iter(current_slice.iter().chain(path.iter()).cloned())
                    });

                    Error {
                        locations: error.locations,
                        path,
                        message: error.message,
                        extensions: error.extensions,
                    }
                })
                .collect();
            let mut data = response.data.unwrap_or_default();
            rewrites::apply_rewrites(schema, &mut data, &self.output_rewrites);
            (Value::from_path(current_dir, data), errors)
        }
    }

    pub(crate) fn operation_kind(&self) -> &OperationKind {
        &self.operation_kind
    }

    pub(crate) fn hash_subquery(
        &mut self,
        subgraph_schemas: &SubgraphSchemas,
        supergraph_schema_hash: &str,
    ) {
        let doc = self.parsed_operation(subgraph_schemas);
        let schema = &subgraph_schemas[self.service_name().as_str()];

        if let Ok(hash) = QueryHashVisitor::hash_query(
            schema,
            supergraph_schema_hash,
            doc,
            self.operation_name.as_deref(),
        ) {
            self.schema_aware_hash = Arc::new(QueryHash(hash));
        }
    }

    pub(crate) fn extract_authorization_metadata(
        &mut self,
        schema: &Valid<apollo_compiler::Schema>,
        global_authorisation_cache_key: &CacheKeyMetadata,
    ) {
        let doc = ExecutableDocument::parse(
            schema,
            self.operation.as_serialized().to_string(),
            "query.graphql",
        )
        // Assume query planing creates a valid document: ignore parse errors
        .unwrap_or_else(|invalid| invalid.partial);
        let subgraph_query_cache_key = AuthorizationPlugin::generate_cache_metadata(
            &doc,
            self.operation_name.as_deref(),
            schema,
            !self.requires.is_empty(),
        );

        // we need to intersect the cache keys because the global key already takes into account
        // the scopes and policies from the client request
        self.authorization = Arc::new(AuthorizationPlugin::intersect_cache_keys_subgraph(
            global_authorisation_cache_key,
            &subgraph_query_cache_key,
        ));
    }

    pub(crate) async fn connector_execution<'a>(
        &'a self,
        parameters: &'a ExecutionParameters<'a>,
        current_dir: &'a Path,
        data: &'a Value,
        sender: tokio::sync::mpsc::Sender<graphql::Response>,
        connector_node: &'a PlanNode,
    ) -> Result<(Value, Vec<Error>), FetchError> {
        let Variables {
            variables,
            inverted_paths: paths,
        } = match Variables::new(
            &self.requires,
            self.variable_usages.as_ref(),
            data,
            current_dir,
            // Needs the original request here
            parameters.supergraph_request,
            parameters.schema,
            &self.input_rewrites,
        ) {
            Some(variables) => variables,
            None => {
                return Ok((Value::Object(Object::default()), Vec::new()));
            }
        };

        let mut request = parameters.supergraph_request.body().clone();
        request.variables = variables;
        let mut supergraph_request = http::Request::builder()
            .method(parameters.supergraph_request.method())
            .uri(parameters.supergraph_request.uri())
            .body(request)
            .unwrap();
        for (name, value) in parameters.supergraph_request.headers() {
            supergraph_request
                .headers_mut()
                .insert(name.clone(), value.clone());
        }

        let subparameters = ExecutionParameters {
            context: parameters.context,
            service_factory: parameters.service_factory,
            schema: parameters.schema,
            deferred_fetches: parameters.deferred_fetches,
            query: parameters.query,
            root_node: parameters.root_node,
            subscription_handle: parameters.subscription_handle,
            subscription_config: parameters.subscription_config,
            supergraph_request: &Arc::new(supergraph_request),
        };

        let path = Path::default();
        let (mut value, errors) = connector_node
            .execute_recursively(&subparameters, &path, data, sender)
            .instrument(tracing::info_span!(
                "connector",
                "graphql.path" = %current_dir,
                "apollo.subgraph.name" = self.service_name.as_str(),
                "otel.kind" = "INTERNAL"
            ))
            .await;

        let magic_finder = match self.protocol.as_ref() {
            Protocol::RestWrapper(wrapper) => wrapper.magic_finder_field.as_ref(),
            _ => None,
        };

        if let Some(magic_finder) = magic_finder {
            let magic_finder = serde_json_bytes::ByteString::from(magic_finder.as_str());
            if let Value::Object(ref mut obj) = value {
                if let Some(v) = obj.remove(&magic_finder) {
                    obj.insert("_entities", v);
                }
            }
        }

        let response = graphql::Response::builder()
            .data(value)
            .errors(errors)
            .build();

        let (value, errors) =
            self.response_at_path(parameters.schema, current_dir, paths, response);
        if let Some(id) = &self.id {
            if let Some(sender) = parameters.deferred_fetches.get(id.as_str()) {
                tracing::info!(monotonic_counter.apollo.router.operations.defer.fetch = 1u64);
                if let Err(e) = sender.clone().send((value.clone(), errors.clone())) {
                    tracing::error!("error sending fetch result at path {} and id {:?} for deferred response building: {}", current_dir, self.id, e);
                }
            }
        }

        Ok((value, errors))
    }

    pub(crate) async fn generate_connector_plan(
        &mut self,
        schema: &Schema,
        subgraph_planners: &HashMap<Arc<String>, Arc<Planner<QueryPlanResult>>>,
        connectors: &Arc<HashMap<Arc<String>, Connector>>,
    ) -> Result<Option<(PlanSuccess<QueryPlanResult>, RestProtocolWrapper)>, QueryPlannerError>
    {
        if let Some(planner) = subgraph_planners.get(&self.service_name.to_string()) {
            tracing::debug!(
                "planning for subgraph '{}' and query '{}'",
                self.service_name,
                self.operation
            );

            let connectors_in_subgraph = connectors
                .iter()
                .filter_map(|(_, connector)| {
                    if *connector.origin_subgraph == self.service_name.as_str() {
                        Some(connector)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();

            let (operation, rest_protocol_wrapper) = if let Some(rest_protocol_wrapper) =
                finder_field_for_fetch_node(
                    schema,
                    &connectors_in_subgraph,
                    self.requires.as_slice(),
                ) {
                if let Some(mff) = &rest_protocol_wrapper.magic_finder_field {
                    (
                        self.operation.replace("_entities", mff),
                        rest_protocol_wrapper,
                    )
                } else {
                    (self.operation.clone(), rest_protocol_wrapper)
                }
            } else {
                (
                    self.operation.clone(),
                    RestProtocolWrapper {
                        connector_service_name: self.service_name.to_string(),
                        magic_finder_field: None,
                    },
                )
            };

            tracing::debug!(
                "replaced with operation(magic finder field={:?}): {operation}",
                rest_protocol_wrapper.magic_finder_field.as_ref()
            );
            match planner
                .plan(
                    operation.to_string(),
                    self.operation_name.as_ref().map(|on| on.to_string()),
                    Default::default(),
                )
                .await
                .map_err(QueryPlannerError::RouterBridgeError)?
                .into_result()
            {
                Ok(mut plan) => {
                    if let Some(node) = plan.data.query_plan.node.as_mut() {
                        node.update_connector_plan(&self.service_name.to_string(), connectors);
                    }

                    return Ok(Some((plan, rest_protocol_wrapper)));
                }
                Err(err) => {
                    return Err(QueryPlannerError::from(err));
                }
            }
        }
        Ok(None)
    }

    // TODO: let's go all in on nodestr
    pub(crate) fn update_connector_plan(
        &mut self,
        parent_service_name: &String,
        connectors: &Arc<HashMap<Arc<String>, Connector>>,
    ) {
        let parent_service_name = parent_service_name.to_string();
        let connector = connectors.get(&self.service_name.to_string()).unwrap(); // TODO
                                                                                 // .map(|c| c.name().into())
                                                                                 // TODO
                                                                                 // .unwrap_or_else(|| String::new().into());
        let service_name =
            std::mem::replace(&mut self.service_name, connector.display_name().into());
        self.protocol = Arc::new(Protocol::RestFetch(RestFetchNode {
            connector_service_name: service_name.to_string(),
            parent_service_name,
        }))
    }
}
