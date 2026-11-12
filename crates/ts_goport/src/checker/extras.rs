//! Small Go functions that live outside the ported ranges (emitresolver.go,
//! nodebuilderimpl.go, symbolaccessibility.go) but that ported checker code
//! calls. Also the checker's synthetic flow nodes.

use crate::prelude::*;

/// File index for flow nodes that the checker creates. Go allocates
/// `&ast.FlowNode{}` values; here a thread-local list owns them.
pub const SYNTHETIC_FLOW_FILE: usize = 0xffff_ffff;

/// The synthetic flow nodes of a thread. An id is `base` plus the place in
/// `flows`, and ids are never used again: a freed node's slot is `None`
/// (a read of it panics), and freed nodes at the front leave the list.
// PORT: not in Go. wasm runs every checker on the loading thread, which
// never ends, so the nodes are freed per program (`free_synthetic_flows_of`)
// instead of with a worker thread: without it each run that refines a
// parameter leaked its nodes (about 2 KB a run in the ts_wasm soaks).
struct SyntheticFlows {
    base: usize,
    /// Each node with the id of the program that was current when it was
    /// made (0 for none).
    flows: std::collections::VecDeque<Option<(u32, Rc<FlowNode>)>>,
}

impl SyntheticFlows {
    /// Drops the freed nodes at the front.
    fn trim(&mut self) {
        while matches!(self.flows.front(), Some(None)) {
            self.flows.pop_front();
            self.base += 1;
        }
    }
}

thread_local! {
    static SYNTHETIC_FLOWS: RefCell<SyntheticFlows> = const {
        RefCell::new(SyntheticFlows {
            base: 0,
            flows: std::collections::VecDeque::new(),
        })
    };
}

/// Reads a flow node that `new_synthetic_flow_node` created.
pub fn synthetic_flow(id: FlowNodeId) -> Rc<FlowNode> {
    SYNTHETIC_FLOWS.with(|f| {
        let f = f.borrow();
        let slot = id
            .local_index()
            .checked_sub(f.base)
            .and_then(|at| f.flows.get(at))
            .and_then(Option::as_ref);
        match slot {
            Some((_, flow)) => Rc::clone(flow),
            None => panic!("synthetic flow node {} was freed", id.local_index()),
        }
    })
}

fn push_synthetic_flow(flow: FlowNode) -> FlowNodeId {
    let program = crate::core::try_prog().map_or(0, |program| program.id);
    SYNTHETIC_FLOWS.with(|f| {
        let mut f = f.borrow_mut();
        f.flows.push_back(Some((program, Rc::new(flow))));
        let index = f.base + f.flows.len() - 1;
        assert!(index < 0xffff_fffe, "too many synthetic flow nodes");
        FlowNodeId::new(SYNTHETIC_FLOW_FILE, index)
    })
}

/// Frees the synthetic flow nodes of this thread. Their ids must not be read
/// again on this thread. A checker worker of a released program calls it
/// after dropping its checker (`program::release_program`).
pub fn free_synthetic_flows() {
    let flows = SYNTHETIC_FLOWS.with(|f| {
        let mut f = f.borrow_mut();
        f.base += f.flows.len();
        std::mem::take(&mut f.flows)
    });
    drop(flows);
}

/// Frees the synthetic flow nodes of this thread that were made while
/// program `program` was current. wasm: its checker pool stops on this
/// thread (`program::release_program`), after its checkers drop.
pub fn free_synthetic_flows_of(program: u32) {
    let freed: Vec<Rc<FlowNode>> = SYNTHETIC_FLOWS.with(|f| {
        let mut f = f.borrow_mut();
        let mut freed = Vec::new();
        for slot in &mut f.flows {
            if slot.as_ref().is_some_and(|(of, _)| *of == program) {
                freed.extend(slot.take().map(|(_, flow)| flow));
            }
        }
        f.trim();
        freed
    });
    // After the borrow: a node can hold the last `Rc` of another.
    drop(freed);
}

/// PORT: not in Go. Test hook: the synthetic flow nodes of this thread that
/// are not freed (the ts_wasm multi-run test).
#[doc(hidden)]
#[must_use]
pub fn live_synthetic_flows() -> usize {
    SYNTHETIC_FLOWS.with(|f| {
        f.borrow()
            .flows
            .iter()
            .filter(|slot| slot.is_some())
            .count()
    })
}

/// Leaks the synthetic flow nodes of this thread instead of freeing them when
/// the thread ends. A checker worker of a one-program process calls it at the
/// end of the process, where freeing only costs time (like the checker, see
/// `program::create_checkers`).
pub fn forget_synthetic_flows() {
    SYNTHETIC_FLOWS.with(|f| {
        let mut f = f.borrow_mut();
        f.base += f.flows.len();
        std::mem::forget(std::mem::take(&mut f.flows));
    });
}

impl Checker {
    /// Go `&ast.FlowNode{Flags: flags, Node: node, Antecedent: antecedent}` in the checker.
    pub fn new_synthetic_flow_node(
        &mut self,
        flags: FlowFlags,
        node: Node,
        antecedent: FlowNodeId,
    ) -> FlowNodeId {
        push_synthetic_flow(FlowNode {
            flags,
            node,
            antecedent,
            antecedents: Vec::new(),
        })
    }

    // Go: checker/emitresolver.go:383 isConstEnumOrConstEnumOnlyModule
    pub fn is_const_enum_or_const_enum_only_module(&self, s: SymbolId) -> bool {
        self.is_const_enum_symbol(s)
            || self
                .sym(s)
                .flags
                .intersects(SymbolFlags::CONST_ENUM_ONLY_MODULE)
    }

    // Go: checker/symbolaccessibility.go:657 compareSymbolChainsWorker
    pub fn compare_symbol_chains_worker(&mut self, a: &[SymbolId], b: &[SymbolId]) -> i32 {
        let chain_len = a.len() as i32 - b.len() as i32;
        if chain_len != 0 {
            return chain_len;
        }
        for idx in 0..a.len() {
            let comparison = self.compare_symbols(a[idx], b[idx]);
            if comparison != 0 {
                return comparison;
            }
        }
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_flow_guard_survives_free() {
        std::thread::spawn(|| {
            let id0 = push_synthetic_flow(FlowNode {
                flags: FlowFlags::START,
                node: Node::NIL,
                antecedent: FlowNodeId::NIL,
                antecedents: Vec::new(),
            });
            let id1 = push_synthetic_flow(FlowNode {
                flags: FlowFlags::TRUE_CONDITION,
                node: Node(1),
                antecedent: id0,
                antecedents: Vec::new(),
            });
            let held = id0.get_flow();
            assert_eq!(held.flags, FlowFlags::START);
            assert_eq!(held.node, Node::NIL);
            assert_eq!(held.antecedent, FlowNodeId::NIL);
            {
                let flow = id1.get_flow();
                assert_eq!(flow.flags, FlowFlags::TRUE_CONDITION);
                assert_eq!(flow.node, Node(1));
                assert_eq!(flow.antecedent, id0);
            }
            free_synthetic_flows();
            assert_eq!(held.flags, FlowFlags::START);
        })
        .join()
        .unwrap();
    }
}

// Go: checker/nodebuilderimpl.go:1194 TryGetModuleSpecifierFromDeclaration
pub fn try_get_module_specifier_from_declaration(node: Node) -> Node {
    let res = try_get_module_specifier_from_declaration_worker(node);
    if res.is_nil() || !is_string_literal(res) {
        return Node::NIL;
    }
    res
}

// Go: checker/nodebuilderimpl.go:1202 tryGetModuleSpecifierFromDeclarationWorker
fn try_get_module_specifier_from_declaration_worker(node: Node) -> Node {
    match node.kind() {
        SyntaxKind::VariableDeclaration | SyntaxKind::BindingElement => {
            let module_call = find_ancestor(node.initializer(), |n| {
                is_require_call(n, true /*requireStringLiteralLikeArgument*/) || is_import_call(n)
            });
            if module_call.is_nil() {
                return Node::NIL;
            }
            module_call.arguments().get(0)
        }
        SyntaxKind::ImportDeclaration
        | SyntaxKind::ExportDeclaration
        | SyntaxKind::JsDocImportTag => node.module_specifier(),
        SyntaxKind::ImportEqualsDeclaration => {
            let r = node.module_reference();
            if r.kind() != SyntaxKind::ExternalModuleReference {
                return Node::NIL;
            }
            r.expression()
        }
        SyntaxKind::ImportClause | SyntaxKind::NamespaceExport => node.parent().module_specifier(),
        SyntaxKind::NamespaceImport | SyntaxKind::ExportSpecifier => {
            node.parent().parent().module_specifier()
        }
        SyntaxKind::ImportSpecifier => node.parent().parent().parent().module_specifier(),
        SyntaxKind::ImportType => {
            if is_literal_import_type_node(node) {
                return node.argument().literal();
            }
            Node::NIL
        }
        _ => {
            crate::gostd::debug::assert_never(&crate::gostd::debug::kind_string(node.kind()), None)
        }
    }
}
