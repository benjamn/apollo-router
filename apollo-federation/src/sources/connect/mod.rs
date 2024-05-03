mod models;
mod query_graph;
mod json_selection;
pub(crate) mod spec;
mod url_path_template;

use crate::error::FederationError;
use crate::schema::position::{
    EnumTypeDefinitionPosition, ObjectFieldDefinitionPosition,
    ObjectOrInterfaceFieldDirectivePosition, ObjectTypeDefinitionPosition,
    ScalarTypeDefinitionPosition,
};

use crate::source_aware::federated_query_graph::graph_path::{
    ConditionResolutionId, OperationPathElement,
};
use crate::source_aware::federated_query_graph::path_tree::FederatedPathTreeChildKey;
use crate::source_aware::federated_query_graph::{FederatedQueryGraph, SelfConditionIndex};
use crate::source_aware::query_plan::{FetchDataPathElement, QueryPlanCost};
use crate::sources::connect::json_selection::{PathSelection, Property, SubSelection};
use crate::sources::{
    SourceFetchDependencyGraphApi, SourceFetchDependencyGraphNode, SourceFetchNode, SourceId,
    SourcePath, SourcePathApi,
};
use crate::ValidFederationSubgraph;
use apollo_compiler::executable::{Name, Value};
use apollo_compiler::NodeStr;
use indexmap::{IndexMap, IndexSet};
use petgraph::graph::EdgeIndex;
pub use json_selection::ApplyTo;
pub use json_selection::ApplyToError;
pub use json_selection::Selection;
use std::sync::Arc;
pub use url_path_template::URLPathTemplate;

pub(crate) use spec::ConnectSpecDefinition;

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub(crate) struct ConnectId {
    subgraph_name: NodeStr,
    directive: ObjectOrInterfaceFieldDirectivePosition,
}

#[derive(Debug)]
pub(crate) struct ConnectFederatedQueryGraph {
    subgraphs_by_name: IndexMap<NodeStr, ValidFederationSubgraph>,
    // source_directives_by_name: IndexMap<NodeStr, SourceDirectiveArguments>,
    // connect_directives_by_source: IndexMap<ConnectId, ConnectDirectiveArguments>,
}

#[derive(Debug)]
pub(crate) struct ConnectFederatedAbstractQueryGraphNode;

#[derive(Debug)]
pub(crate) enum ConnectFederatedConcreteQueryGraphNode {
    ConnectParent {
        subgraph_type: ObjectTypeDefinitionPosition,
    },
    SelectionRoot {
        subgraph_type: ObjectTypeDefinitionPosition,
        property_path: Vec<Property>,
    },
    SelectionChild {
        subgraph_type: ObjectTypeDefinitionPosition,
    },
}

#[derive(Debug)]
pub(crate) enum ConnectFederatedEnumQueryGraphNode {
    SelectionRoot {
        subgraph_type: EnumTypeDefinitionPosition,
        property_path: Vec<Property>,
    },
    SelectionChild {
        subgraph_type: EnumTypeDefinitionPosition,
    },
}

#[derive(Debug)]
pub(crate) enum ConnectFederatedScalarQueryGraphNode {
    SelectionRoot {
        subgraph_type: ScalarTypeDefinitionPosition,
        property_path: Vec<Property>,
    },
    CustomScalarSelectionRoot {
        subgraph_type: ScalarTypeDefinitionPosition,
        selection: Selection,
    },
    SelectionChild {
        subgraph_type: ScalarTypeDefinitionPosition,
    },
}

#[derive(Debug)]
pub(crate) struct ConnectFederatedAbstractFieldQueryGraphEdge;

#[derive(Debug)]

pub(crate) enum ConnectFederatedConcreteFieldQueryGraphEdge {
    Connect {
        subgraph_field: ObjectFieldDefinitionPosition,
    },
    Selection {
        subgraph_field: ObjectFieldDefinitionPosition,
        property_path: Vec<Property>,
    },
    CustomScalarPathSelection {
        subgraph_field: ObjectFieldDefinitionPosition,
        path_selection: PathSelection,
    },
    CustomScalarStarSelection {
        subgraph_field: ObjectFieldDefinitionPosition,
        star_subselection: Option<SubSelection>,
        excluded_properties: IndexSet<Property>,
    },
}

#[derive(Debug)]
pub(crate) struct ConnectFederatedTypeConditionQueryGraphEdge;

#[derive(Debug)]
pub(crate) enum ConnectFederatedSourceEnteringQueryGraphEdge {
    ConnectParent {
        subgraph_type: ObjectTypeDefinitionPosition,
    },
}

/// Connect-aware query graph builder
///
/// This builder is in charge of setting up nodes / edges in the query graph
/// that correspond to REST mappings defined through the @source and @connect
/// directives.
///
/// Refer to [SourceSpecDefinition] and [ConnectSpecDefinition] for more info.
pub(crate) struct ConnectFederatedQueryGraphBuilder;

#[derive(Debug)]
pub(crate) struct ConnectFetchDependencyGraph;

impl SourceFetchDependencyGraphApi for ConnectFetchDependencyGraph {
    fn can_reuse_node<'path_tree>(
        &self,
        _query_graph: Arc<FederatedQueryGraph>,
        _merge_at: &[FetchDataPathElement],
        _source_entering_edge: EdgeIndex,
        _path_tree_edges: Vec<&'path_tree FederatedPathTreeChildKey>,
        _source_data: &SourceFetchDependencyGraphNode,
    ) -> Result<Vec<&'path_tree FederatedPathTreeChildKey>, FederationError> {
        todo!()
    }

    fn add_node<'path_tree>(
        &self,
        _query_graph: Arc<FederatedQueryGraph>,
        _merge_at: &[FetchDataPathElement],
        _source_entering_edge: EdgeIndex,
        _self_condition_resolution: Option<ConditionResolutionId>,
        _path_tree_edges: Vec<&'path_tree FederatedPathTreeChildKey>,
    ) -> Result<
        (
            SourceFetchDependencyGraphNode,
            Vec<&'path_tree FederatedPathTreeChildKey>,
        ),
        FederationError,
    > {
        todo!()
    }

    fn new_path(
        &self,
        _query_graph: Arc<FederatedQueryGraph>,
        _merge_at: &[FetchDataPathElement],
        _source_entering_edge: EdgeIndex,
        _self_condition_resolution: Option<ConditionResolutionId>,
    ) -> Result<SourcePath, FederationError> {
        todo!()
    }

    fn add_path(
        &self,
        _query_graph: Arc<FederatedQueryGraph>,
        _source_path: SourcePath,
        _source_data: &mut SourceFetchDependencyGraphNode,
    ) -> Result<(), FederationError> {
        todo!()
    }

    fn to_cost(
        &self,
        _query_graph: Arc<FederatedQueryGraph>,
        _source_id: SourceId,
        _source_data: &SourceFetchDependencyGraphNode,
    ) -> Result<QueryPlanCost, FederationError> {
        todo!()
    }

    fn to_plan_node(
        &self,
        _query_graph: Arc<FederatedQueryGraph>,
        _source_id: SourceId,
        _source_data: &SourceFetchDependencyGraphNode,
        _fetch_count: u32,
    ) -> Result<SourceFetchNode, FederationError> {
        todo!()
    }
}

#[derive(Debug)]
pub(crate) struct ConnectFetchDependencyGraphNode {
    merge_at: Vec<FetchDataPathElement>,
    source_entering_edge: EdgeIndex,
    field_response_name: Name,
    field_arguments: IndexMap<Name, Value>,
    selection: Selection,
}

#[derive(Debug)]
pub(crate) struct ConnectPath {
    merge_at: Vec<FetchDataPathElement>,
    source_entering_edge: EdgeIndex,
    source_id: SourceId,
    field: Option<ConnectPathField>,
}

#[derive(Debug)]
pub(crate) struct ConnectPathField {
    response_name: Name,
    arguments: IndexMap<Name, Value>,
    selections: ConnectPathSelections,
}

#[derive(Debug)]
pub(crate) enum ConnectPathSelections {
    Selections {
        head_property_path: Vec<Property>,
        named_selections: Vec<(Name, Vec<Property>)>,
        tail_selection: Option<(Name, ConnectPathTailSelection)>,
    },
    CustomScalarRoot {
        selection: Selection,
    },
}

#[derive(Debug)]
pub(crate) enum ConnectPathTailSelection {
    Selection {
        property_path: Vec<Property>,
    },
    CustomScalarPathSelection {
        path_selection: PathSelection,
    },
    CustomScalarStarSelection {
        star_subselection: Option<SubSelection>,
        excluded_properties: IndexSet<Property>,
    },
}

impl SourcePathApi for ConnectPath {
    fn source_id(&self) -> &SourceId {
        todo!()
    }

    fn add_operation_element(
        &self,
        _query_graph: Arc<FederatedQueryGraph>,
        _operation_element: Arc<OperationPathElement>,
        _edge: Option<EdgeIndex>,
        _self_condition_resolutions: IndexMap<SelfConditionIndex, ConditionResolutionId>,
    ) -> Result<SourcePath, FederationError> {
        todo!()
    }
}

#[derive(Debug)]
pub struct ConnectFetchNode {
    source_id: ConnectId,
    field_response_name: Name,
    field_arguments: IndexMap<Name, Value>,
    selection: Selection,
}
