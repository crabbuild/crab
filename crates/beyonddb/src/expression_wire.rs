//! Deterministic Cell wire form for ExtendDB expression syntax trees.

use std::collections::BTreeMap;

use extenddb_core::expression::{
    ArithOp, CompareOp, Expr, ExpressionMaps, PathElement, UpdateAction, apply_update_validated,
    evaluate_condition,
};
use extenddb_core::types::{AttributeDefinition, AttributeValue, Item};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct WireMaps {
    names: BTreeMap<String, String>,
    values: BTreeMap<String, AttributeValue>,
}

impl WireMaps {
    fn from_core(maps: &ExpressionMaps) -> Self {
        Self {
            names: maps
                .names
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            values: maps
                .values
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        }
    }

    fn to_core(&self) -> ExpressionMaps {
        ExpressionMaps::new(
            self.names
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            self.values
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        )
    }
}

/// Canonical Cell wire form of an ExtendDB condition and its value maps.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WireCondition {
    expression: WireExpr,
    maps: WireMaps,
}

impl WireCondition {
    pub(crate) fn from_core(expression: &Expr, maps: &ExpressionMaps) -> Self {
        Self {
            expression: WireExpr::from_core(expression),
            maps: WireMaps::from_core(maps),
        }
    }

    pub(crate) fn evaluate(&self, item: &Item) -> std::result::Result<bool, String> {
        evaluate_condition(
            &self.expression.clone().into_core(),
            item,
            &self.maps.to_core(),
        )
        .map_err(|error| error.to_string())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
enum WirePathElement {
    Attribute(String),
    Index(usize),
}

impl WirePathElement {
    fn from_core(element: &PathElement) -> Self {
        match element {
            PathElement::Attribute(name) => Self::Attribute(name.clone()),
            PathElement::Index(index) => Self::Index(*index),
        }
    }

    fn into_core(self) -> PathElement {
        match self {
            Self::Attribute(name) => PathElement::Attribute(name),
            Self::Index(index) => PathElement::Index(index),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
enum WireCompareOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl From<CompareOp> for WireCompareOp {
    fn from(value: CompareOp) -> Self {
        match value {
            CompareOp::Eq => Self::Eq,
            CompareOp::Ne => Self::Ne,
            CompareOp::Lt => Self::Lt,
            CompareOp::Le => Self::Le,
            CompareOp::Gt => Self::Gt,
            CompareOp::Ge => Self::Ge,
        }
    }
}

impl From<WireCompareOp> for CompareOp {
    fn from(value: WireCompareOp) -> Self {
        match value {
            WireCompareOp::Eq => Self::Eq,
            WireCompareOp::Ne => Self::Ne,
            WireCompareOp::Lt => Self::Lt,
            WireCompareOp::Le => Self::Le,
            WireCompareOp::Gt => Self::Gt,
            WireCompareOp::Ge => Self::Ge,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
enum WireArithOp {
    Add,
    Sub,
}

impl From<ArithOp> for WireArithOp {
    fn from(value: ArithOp) -> Self {
        match value {
            ArithOp::Add => Self::Add,
            ArithOp::Sub => Self::Sub,
        }
    }
}

impl From<WireArithOp> for ArithOp {
    fn from(value: WireArithOp) -> Self {
        match value {
            WireArithOp::Add => Self::Add,
            WireArithOp::Sub => Self::Sub,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
enum WireExpr {
    Path(Vec<WirePathElement>),
    Placeholder(String),
    Compare {
        left: Box<WireExpr>,
        op: WireCompareOp,
        right: Box<WireExpr>,
    },
    And(Box<WireExpr>, Box<WireExpr>),
    Or(Box<WireExpr>, Box<WireExpr>),
    Not(Box<WireExpr>),
    Function {
        name: String,
        args: Vec<WireExpr>,
    },
    Arithmetic {
        left: Box<WireExpr>,
        op: WireArithOp,
        right: Box<WireExpr>,
    },
    Between {
        operand: Box<WireExpr>,
        low: Box<WireExpr>,
        high: Box<WireExpr>,
    },
    In {
        operand: Box<WireExpr>,
        list: Vec<WireExpr>,
    },
}

impl WireExpr {
    fn from_core(expr: &Expr) -> Self {
        match expr {
            Expr::Path(path) => Self::Path(path.iter().map(WirePathElement::from_core).collect()),
            Expr::Placeholder(name) => Self::Placeholder(name.clone()),
            Expr::Compare { left, op, right } => Self::Compare {
                left: Box::new(Self::from_core(left)),
                op: (*op).into(),
                right: Box::new(Self::from_core(right)),
            },
            Expr::And(left, right) => Self::And(
                Box::new(Self::from_core(left)),
                Box::new(Self::from_core(right)),
            ),
            Expr::Or(left, right) => Self::Or(
                Box::new(Self::from_core(left)),
                Box::new(Self::from_core(right)),
            ),
            Expr::Not(inner) => Self::Not(Box::new(Self::from_core(inner))),
            Expr::Function { name, args } => Self::Function {
                name: name.clone(),
                args: args.iter().map(Self::from_core).collect(),
            },
            Expr::Arithmetic { left, op, right } => Self::Arithmetic {
                left: Box::new(Self::from_core(left)),
                op: (*op).into(),
                right: Box::new(Self::from_core(right)),
            },
            Expr::Between { operand, low, high } => Self::Between {
                operand: Box::new(Self::from_core(operand)),
                low: Box::new(Self::from_core(low)),
                high: Box::new(Self::from_core(high)),
            },
            Expr::In { operand, list } => Self::In {
                operand: Box::new(Self::from_core(operand)),
                list: list.iter().map(Self::from_core).collect(),
            },
        }
    }

    fn into_core(self) -> Expr {
        match self {
            Self::Path(path) => {
                Expr::Path(path.into_iter().map(WirePathElement::into_core).collect())
            }
            Self::Placeholder(name) => Expr::Placeholder(name),
            Self::Compare { left, op, right } => Expr::Compare {
                left: Box::new(left.into_core()),
                op: op.into(),
                right: Box::new(right.into_core()),
            },
            Self::And(left, right) => {
                Expr::And(Box::new(left.into_core()), Box::new(right.into_core()))
            }
            Self::Or(left, right) => {
                Expr::Or(Box::new(left.into_core()), Box::new(right.into_core()))
            }
            Self::Not(inner) => Expr::Not(Box::new(inner.into_core())),
            Self::Function { name, args } => Expr::Function {
                name,
                args: args.into_iter().map(Self::into_core).collect(),
            },
            Self::Arithmetic { left, op, right } => Expr::Arithmetic {
                left: Box::new(left.into_core()),
                op: op.into(),
                right: Box::new(right.into_core()),
            },
            Self::Between { operand, low, high } => Expr::Between {
                operand: Box::new(operand.into_core()),
                low: Box::new(low.into_core()),
                high: Box::new(high.into_core()),
            },
            Self::In { operand, list } => Expr::In {
                operand: Box::new(operand.into_core()),
                list: list.into_iter().map(Self::into_core).collect(),
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
enum WireUpdateAction {
    Set {
        path: Vec<WirePathElement>,
        value: WireExpr,
    },
    Remove {
        path: Vec<WirePathElement>,
    },
    Add {
        path: Vec<WirePathElement>,
        value: WireExpr,
    },
    Delete {
        path: Vec<WirePathElement>,
        value: WireExpr,
    },
}

impl WireUpdateAction {
    fn from_core(action: &UpdateAction) -> Self {
        match action {
            UpdateAction::Set { path, value } => Self::Set {
                path: path.iter().map(WirePathElement::from_core).collect(),
                value: WireExpr::from_core(value),
            },
            UpdateAction::Remove { path } => Self::Remove {
                path: path.iter().map(WirePathElement::from_core).collect(),
            },
            UpdateAction::Add { path, value } => Self::Add {
                path: path.iter().map(WirePathElement::from_core).collect(),
                value: WireExpr::from_core(value),
            },
            UpdateAction::Delete { path, value } => Self::Delete {
                path: path.iter().map(WirePathElement::from_core).collect(),
                value: WireExpr::from_core(value),
            },
        }
    }

    fn into_core(self) -> UpdateAction {
        let path =
            |path: Vec<WirePathElement>| path.into_iter().map(WirePathElement::into_core).collect();
        match self {
            Self::Set { path: p, value } => UpdateAction::Set {
                path: path(p),
                value: value.into_core(),
            },
            Self::Remove { path: p } => UpdateAction::Remove { path: path(p) },
            Self::Add { path: p, value } => UpdateAction::Add {
                path: path(p),
                value: value.into_core(),
            },
            Self::Delete { path: p, value } => UpdateAction::Delete {
                path: path(p),
                value: value.into_core(),
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct WireUpdate {
    actions: Vec<WireUpdateAction>,
    maps: WireMaps,
}

impl WireUpdate {
    pub(crate) fn from_core(actions: &[UpdateAction], maps: &ExpressionMaps) -> Self {
        Self {
            actions: actions.iter().map(WireUpdateAction::from_core).collect(),
            maps: WireMaps::from_core(maps),
        }
    }

    pub(crate) fn apply(
        &self,
        item: &mut Item,
        definitions: &[AttributeDefinition],
    ) -> std::result::Result<(), String> {
        let actions: Vec<UpdateAction> = self
            .actions
            .iter()
            .cloned()
            .map(WireUpdateAction::into_core)
            .collect();
        apply_update_validated(&actions, item, &self.maps.to_core(), &[], definitions)
            .map_err(|error| error.to_string())
    }
}
