#![allow(clippy::arithmetic_side_effects)]
//! Static Byte Code Analysis

use crate::disassembler::disassemble_instruction;
use crate::{
    ebpf,
    elf::Executable,
    error::EbpfError,
    vm::{ContextObject, DynamicAnalysis, TestContextObject},
};
use rustc_demangle::demangle;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// Register state recorded after executing one instruction
///
/// The last register is the program counter (aka pc).
pub type TraceLogEntry = [u64; 12];

/// Used for topological sort
#[derive(PartialEq, Eq, Debug)]
pub struct TopologicalIndex {
    /// Strongly connected component ID
    pub scc_id: usize,
    /// Discovery order inside a strongly connected component
    pub discovery: usize,
}

impl Default for TopologicalIndex {
    fn default() -> Self {
        Self {
            scc_id: usize::MAX,
            discovery: usize::MAX,
        }
    }
}

impl Ord for TopologicalIndex {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.scc_id.cmp(&other.scc_id)).then(self.discovery.cmp(&other.discovery))
    }
}

impl PartialOrd for TopologicalIndex {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// A node of the control-flow graph
#[derive(Debug)]
pub struct CfgNode {
    /// Human readable name
    pub label: String,
    /// Predecessors which can jump to the start of this basic block
    pub sources: Vec<usize>,
    /// Successors which the end of this basic block can jump to
    pub destinations: Vec<usize>,
    /// Range of the instructions belonging to this basic block
    pub instructions: std::ops::Range<usize>,
    /// Topological index
    pub topo_index: TopologicalIndex,
    /// Immediate dominator (the last control flow junction)
    pub dominator_parent: usize,
    /// All basic blocks which can only be reached through this one
    pub dominated_children: Vec<usize>,
}

/// An instruction or Φ node of the data-flow graph
#[derive(PartialOrd, Ord, PartialEq, Eq, Hash, Clone, Debug)]
pub enum DfgNode {
    /// Points to a single instruction
    InstructionNode(usize),
    /// Points to a basic block which starts with a Φ node (because it has multiple CFG sources)
    PhiNode(usize),
}

/// The register or memory location a data-flow edge guards
#[derive(PartialOrd, Ord, PartialEq, Eq, Hash, Clone, Debug)]
pub enum DataResource {
    /// A BPF register
    Register(u8),
    /// A (potentially writeable) memory location
    Memory,
}

/// The kind of a data-flow edge
#[derive(PartialOrd, Ord, PartialEq, Eq, Hash, Clone, Debug)]
pub enum DfgEdgeKind {
    /// This kind represents data-flow edges which actually carry data
    ///
    /// E.g. the destination reads a resource, written by the source.
    Filled,
    /// This kind incurrs no actual data-flow
    ///
    /// E.g. the destination overwrites a resource, written by the source.
    Empty,
}

/// An edge of the data-flow graph
#[derive(PartialOrd, Ord, PartialEq, Eq, Hash, Clone, Debug)]
pub struct DfgEdge {
    /// The DfgNode that the destination depends on
    pub source: DfgNode,
    /// The DfgNode that depends on the source
    pub destination: DfgNode,
    /// Write-read or write-write
    pub kind: DfgEdgeKind,
    /// A register or memory location
    pub resource: DataResource,
}

impl Default for CfgNode {
    fn default() -> Self {
        Self {
            label: String::new(),
            sources: Vec::new(),
            destinations: Vec::new(),
            instructions: 0..0,
            topo_index: TopologicalIndex::default(),
            dominator_parent: usize::MAX,
            dominated_children: Vec::new(),
        }
    }
}

/// Result of the executable analysis
pub struct Analysis<'a> {
    /// The program which is analyzed
    executable: &'a Executable<TestContextObject>,
    /// Plain list of instructions as they occur in the executable
    pub instructions: Vec<ebpf::Insn>,
    /// Functions in the executable
    pub functions: BTreeMap<usize, (u32, String)>,
    /// Nodes of the control-flow graph
    pub cfg_nodes: BTreeMap<usize, CfgNode>,
    /// Topological order of cfg_nodes
    pub topological_order: Vec<usize>,
    /// CfgNode where the execution starts
    pub entrypoint: usize,
    /// Virtual CfgNode that reaches all functions
    pub super_root: usize,
    /// Data flow edges (the keys are DfgEdge sources)
    pub dfg_forward_edges: BTreeMap<DfgNode, BTreeSet<DfgEdge>>,
    /// Data flow edges (the keys are DfgEdge destinations)
    pub dfg_reverse_edges: BTreeMap<DfgNode, BTreeSet<DfgEdge>>,
}

impl<'a> Analysis<'a> {
    /// Analyze an executable statically
    pub fn from_executable<C: ContextObject>(
        executable: &'a Executable<C>,
    ) -> Result<Self, EbpfError> {
        let (_program_vm_addr, program) = executable.get_text_bytes();
        let mut functions = BTreeMap::new();
        for (key, (function_name, pc)) in executable.get_function_registry().iter() {
            functions.insert(
                pc,
                (key, String::from_utf8_lossy(function_name).to_string()),
            );
        }
        debug_assert!(
            program.len() % ebpf::INSN_SIZE == 0,
            "eBPF program length must be a multiple of {:?} octets is {:?}",
            ebpf::INSN_SIZE,
            program.len()
        );
        let mut instructions = Vec::with_capacity(program.len() / ebpf::INSN_SIZE);
        let mut insn_ptr: usize = 0;
        while insn_ptr * ebpf::INSN_SIZE < program.len() {
            let mut insn = ebpf::get_insn_unchecked(program, insn_ptr);
            if insn.opc == ebpf::LD_DW_IMM {
                insn_ptr += 1;
                if insn_ptr * ebpf::INSN_SIZE >= program.len() {
                    break;
                }
                ebpf::augment_lddw_unchecked(program, &mut insn);
            }
            instructions.push(insn);
            insn_ptr += 1;
        }
        let mut result = Self {
            // Removes the generic ContextObject which is safe because we are not going to execute the program
            executable: unsafe {
                std::mem::transmute::<&Executable<C>, &Executable<TestContextObject>>(executable)
            },
            instructions,
            functions,
            cfg_nodes: BTreeMap::new(),
            topological_order: Vec::new(),
            entrypoint: executable.get_entrypoint_instruction_offset(),
            super_root: insn_ptr,
            dfg_forward_edges: BTreeMap::new(),
            dfg_reverse_edges: BTreeMap::new(),
        };
        result.split_into_basic_blocks(false);
        result.control_flow_graph_tarjan();
        result.control_flow_graph_dominance_hierarchy();
        result.label_basic_blocks();
        let basic_block_outputs = result.intra_basic_block_data_flow();
        result.inter_basic_block_data_flow(basic_block_outputs);
        Ok(result)
    }

    fn link_cfg_edges(&mut self, cfg_edges: Vec<(usize, Vec<usize>)>, both_directions: bool) {
        for (source, destinations) in cfg_edges {
            if both_directions {
                self.cfg_nodes
                    .get_mut(&source)
                    .unwrap()
                    .destinations
                    .clone_from(&destinations);
            }
            for destination in &destinations {
                self.cfg_nodes
                    .get_mut(destination)
                    .unwrap()
                    .sources
                    .push(source);
            }
        }
    }

    /// Splits the sequence of instructions into basic blocks
    ///
    /// Also links the control-flow graph edges between the basic blocks.
    pub fn split_into_basic_blocks(&mut self, flatten_call_graph: bool) {
        self.cfg_nodes.insert(0, CfgNode::default());
        for pc in self.functions.keys() {
            self.cfg_nodes.entry(*pc).or_default();
        }
        let mut cfg_edges = BTreeMap::new();
        for insn in self.instructions.iter() {
            let target_pc = (insn.ptr as isize + insn.off as isize + 1) as usize;
            match insn.opc {
                ebpf::CALL_IMM => {
                    if let Some((function_name, _function)) = self
                        .executable
                        .get_loader()
                        .get_function_registry()
                        .lookup_by_key(insn.imm as u32)
                    {
                        if function_name == b"abort" {
                            self.cfg_nodes.entry(insn.ptr + 1).or_default();
                            cfg_edges.insert(insn.ptr, (insn.opc, Vec::new()));
                        }
                    } else if let Some((_function_name, target_pc)) = self
                        .executable
                        .get_function_registry()
                        .lookup_by_key(insn.imm as u32)
                    {
                        self.cfg_nodes.entry(insn.ptr + 1).or_default();
                        self.cfg_nodes.entry(target_pc).or_default();
                        let destinations = if flatten_call_graph {
                            vec![insn.ptr + 1, target_pc]
                        } else {
                            vec![insn.ptr + 1]
                        };
                        cfg_edges.insert(insn.ptr, (insn.opc, destinations));
                    }
                }
                ebpf::CALL_REG => {
                    // Abnormal CFG edge
                    self.cfg_nodes.entry(insn.ptr + 1).or_default();
                    let destinations = if flatten_call_graph {
                        vec![insn.ptr + 1, self.super_root]
                    } else {
                        vec![insn.ptr + 1]
                    };
                    cfg_edges.insert(insn.ptr, (insn.opc, destinations));
                }
                ebpf::EXIT => {
                    self.cfg_nodes.entry(insn.ptr + 1).or_default();
                    cfg_edges.insert(insn.ptr, (insn.opc, Vec::new()));
                }
                ebpf::JA => {
                    self.cfg_nodes.entry(insn.ptr + 1).or_default();
                    self.cfg_nodes.entry(target_pc).or_default();
                    cfg_edges.insert(insn.ptr, (insn.opc, vec![target_pc]));
                }
                ebpf::JEQ_IMM
                | ebpf::JGT_IMM
                | ebpf::JGE_IMM
                | ebpf::JLT_IMM
                | ebpf::JLE_IMM
                | ebpf::JSET_IMM
                | ebpf::JNE_IMM
                | ebpf::JSGT_IMM
                | ebpf::JSGE_IMM
                | ebpf::JSLT_IMM
                | ebpf::JSLE_IMM
                | ebpf::JEQ_REG
                | ebpf::JGT_REG
                | ebpf::JGE_REG
                | ebpf::JLT_REG
                | ebpf::JLE_REG
                | ebpf::JSET_REG
                | ebpf::JNE_REG
                | ebpf::JSGT_REG
                | ebpf::JSGE_REG
                | ebpf::JSLT_REG
                | ebpf::JSLE_REG => {
                    self.cfg_nodes.entry(insn.ptr + 1).or_default();
                    self.cfg_nodes.entry(target_pc).or_default();
                    cfg_edges.insert(insn.ptr, (insn.opc, vec![insn.ptr + 1, target_pc]));
                }
                _ => {}
            }
        }
        {
            let mut cfg_nodes = BTreeMap::new();
            std::mem::swap(&mut self.cfg_nodes, &mut cfg_nodes);
            let mut cfg_nodes = cfg_nodes
                .into_iter()
                .filter(|(cfg_node_start, _cfg_node)| {
                    match self
                        .instructions
                        .binary_search_by(|insn| insn.ptr.cmp(cfg_node_start))
                    {
                        Ok(_) => true,
                        Err(_index) => false,
                    }
                })
                .collect();
            std::mem::swap(&mut self.cfg_nodes, &mut cfg_nodes);
            for cfg_edge in cfg_edges.values_mut() {
                cfg_edge
                    .1
                    .retain(|destination| self.cfg_nodes.contains_key(destination));
            }
            let mut functions = BTreeMap::new();
            std::mem::swap(&mut self.functions, &mut functions);
            let mut functions = functions
                .into_iter()
                .filter(|(function_start, _)| self.cfg_nodes.contains_key(function_start))
                .collect();
            std::mem::swap(&mut self.functions, &mut functions);
        }
        {
            let mut instruction_index = 0;
            let mut cfg_node_iter = self.cfg_nodes.iter_mut().peekable();
            let mut cfg_edge_iter = cfg_edges.iter_mut().peekable();
            while let Some((cfg_node_start, cfg_node)) = cfg_node_iter.next() {
                let cfg_node_end = if let Some(next_cfg_node) = cfg_node_iter.peek() {
                    *next_cfg_node.0 - 1
                } else {
                    self.instructions.last().unwrap().ptr
                };
                cfg_node.instructions.start = instruction_index;
                while instruction_index < self.instructions.len() {
                    if self.instructions[instruction_index].ptr <= cfg_node_end {
                        instruction_index += 1;
                        cfg_node.instructions.end = instruction_index;
                    } else {
                        break;
                    }
                }
                if let Some(next_cfg_edge) = cfg_edge_iter.peek() {
                    if *next_cfg_edge.0 <= cfg_node_end {
                        cfg_node.destinations.clone_from(&next_cfg_edge.1 .1);
                        cfg_edge_iter.next();
                        continue;
                    }
                }
                if let Some(next_cfg_node) = cfg_node_iter.peek() {
                    if !self.functions.contains_key(cfg_node_start) {
                        cfg_node.destinations.push(*next_cfg_node.0);
                    }
                }
            }
        }
        self.link_cfg_edges(
            self.cfg_nodes
                .iter()
                .map(|(source, cfg_node)| (*source, cfg_node.destinations.clone()))
                .collect::<Vec<(usize, Vec<usize>)>>(),
            false,
        );
        if flatten_call_graph {
            let mut destinations = Vec::new();
            let mut cfg_edges = Vec::new();
            for (source, cfg_node) in self.cfg_nodes.iter() {
                if self.functions.contains_key(source) {
                    destinations = cfg_node
                        .sources
                        .iter()
                        .map(|destination| {
                            self.instructions
                                [self.cfg_nodes.get(destination).unwrap().instructions.end]
                                .ptr
                        })
                        .collect();
                }
                if cfg_node.destinations.is_empty()
                    && self.instructions[cfg_node.instructions.end - 1].opc == ebpf::EXIT
                {
                    cfg_edges.push((*source, destinations.clone()));
                }
            }
            self.link_cfg_edges(cfg_edges, true);
        }
    }

    /// Gives the basic blocks names
    pub fn label_basic_blocks(&mut self) {
        for (pc, cfg_node) in self.cfg_nodes.iter_mut() {
            cfg_node.label = if let Some(function) = self.functions.get(pc) {
                demangle(&function.1).to_string()
            } else {
                format!("lbb_{pc}")
            };
        }
        if let Some(super_root) = self.cfg_nodes.get_mut(&self.super_root) {
            super_root.label = "super_root".to_string();
        }
    }

    /// Generates labels for assembler code
    pub fn disassemble_label<W: std::io::Write>(
        &self,
        output: &mut W,
        suppress_extra_newlines: bool,
        pc: usize,
        last_basic_block: &mut usize,
    ) -> std::io::Result<()> {
        if let Some(cfg_node) = self.cfg_nodes.get(&pc) {
            let is_function = self.functions.contains_key(&pc);
            if is_function || cfg_node.sources != vec![*last_basic_block] {
                if is_function && !suppress_extra_newlines {
                    writeln!(output)?;
                }
                writeln!(output, "{}:", cfg_node.label)?;
            }
            let last_insn = &self.instructions[cfg_node.instructions.end - 1];
            *last_basic_block = if last_insn.opc == ebpf::JA {
                usize::MAX
            } else {
                pc
            };
        }
        Ok(())
    }

    /// Generates assembler code for a single instruction
    pub fn disassemble_instruction(&self, insn: &ebpf::Insn) -> String {
        disassemble_instruction(
            insn,
            &self.cfg_nodes,
            self.executable.get_function_registry(),
            self.executable.get_loader(),
            self.executable.get_sbpf_version(),
        )
    }

    /// Generates assembler code for the analyzed executable
    pub fn disassemble<W: std::io::Write>(&self, output: &mut W) -> std::io::Result<()> {
        let mut last_basic_block = usize::MAX;
        for insn in self.instructions.iter() {
            self.disassemble_label(
                output,
                Some(insn) == self.instructions.first(),
                insn.ptr,
                &mut last_basic_block,
            )?;
            writeln!(output, "    {}", self.disassemble_instruction(insn))?;
        }
        Ok(())
    }

    /// Use this method to print the trace log
    pub fn disassemble_trace_log<W: std::io::Write>(
        &self,
        output: &mut W,
        trace_log: &[TraceLogEntry],
    ) -> Result<(), std::io::Error> {
        let mut pc_to_insn_index = vec![
            0usize;
            self.instructions
                .last()
                .map(|insn| insn.ptr + 2)
                .unwrap_or(0)
        ];
        for (index, insn) in self.instructions.iter().enumerate() {
            pc_to_insn_index[insn.ptr] = index;
            pc_to_insn_index[insn.ptr + 1] = index;
        }
        for (index, entry) in trace_log.iter().enumerate() {
            let pc = entry[11] as usize;
            let insn = &self.instructions[pc_to_insn_index[pc]];
            writeln!(
                output,
                "{:5?} {:016X?} {:5?}: {}",
                index,
                &entry[0..11],
                pc,
                self.disassemble_instruction(insn),
            )?;
        }
        Ok(())
    }

    /// Iterates over the cfg_nodes while providing the PC range of the function they belong to.
    pub fn iter_cfg_by_function(
        &self,
    ) -> impl Iterator<Item = (std::ops::Range<usize>, usize, &CfgNode)> + '_ {
        let mut function_iter = self.functions.keys().peekable();
        let mut function_start = *function_iter.next().unwrap();
        self.cfg_nodes
            .iter()
            .map(move |(cfg_node_start, cfg_node)| {
                if Some(&cfg_node_start) == function_iter.peek() {
                    function_start = *function_iter.next().unwrap();
                }
                let function_end = if let Some(next_function) = function_iter.peek() {
                    **next_function
                } else {
                    self.instructions.last().unwrap().ptr + 1
                };
                (function_start..function_end, *cfg_node_start, cfg_node)
            })
    }

    /// Generates a graphviz DOT of the analyzed executable
    pub fn visualize_graphically<W: std::io::Write>(
        &self,
        output: &mut W,
        dynamic_analysis: Option<&DynamicAnalysis>,
    ) -> std::io::Result<()> {
        fn html_escape(string: &str) -> String {
            string
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('\"', "&quot;")
        }
        fn emit_cfg_node<W: std::io::Write>(
            output: &mut W,
            dynamic_analysis: Option<&DynamicAnalysis>,
            analysis: &Analysis,
            function_range: std::ops::Range<usize>,
            alias_nodes: &mut HashSet<usize>,
            cfg_node_start: usize,
        ) -> std::io::Result<()> {
            let cfg_node = &analysis.cfg_nodes[&cfg_node_start];
            writeln!(output, "    lbb_{} [label=<<table border=\"0\" cellborder=\"0\" cellpadding=\"3\">{}</table>>];",
                cfg_node_start,
                analysis.instructions[cfg_node.instructions.clone()].iter()
                .map(|insn| {
                    let desc = analysis.disassemble_instruction(
                        insn
                    );
                    if let Some(split_index) = desc.find(' ') {
                        let mut rest = desc[split_index+1..].to_string();
                        if rest.len() > MAX_CELL_CONTENT_LENGTH + 1 {
                            rest.truncate(MAX_CELL_CONTENT_LENGTH);
                            rest = format!("{rest}…");
                        }
                        format!("<tr><td align=\"left\">{}</td><td align=\"left\">{}</td></tr>", html_escape(&desc[..split_index]), html_escape(&rest))
                    } else {
                        format!("<tr><td align=\"left\">{}</td></tr>", html_escape(&desc))
                    }
                })
                .collect::<String>()
            )?;
            if let Some(dynamic_analysis) = dynamic_analysis {
                if let Some(recorded_edges) = dynamic_analysis.edges.get(&cfg_node_start) {
                    for destination in recorded_edges.keys() {
                        if !function_range.contains(destination) {
                            alias_nodes.insert(*destination);
                        }
                    }
                }
            }
            for child in &cfg_node.dominated_children {
                emit_cfg_node(
                    output,
                    dynamic_analysis,
                    analysis,
                    function_range.clone(),
                    alias_nodes,
                    *child,
                )?;
            }
            Ok(())
        }
        writeln!(
            output,
            "digraph {{
  graph [
    rankdir=LR;
    concentrate=True;
    style=filled;
    color=lightgrey;
  ];
  node [
    shape=rect;
    style=filled;
    fillcolor=white;
    fontname=\"Courier New\";
  ];
  edge [
    fontname=\"Courier New\";
  ];"
        )?;
        const MAX_CELL_CONTENT_LENGTH: usize = 15;
        let mut function_iter = self.functions.keys().peekable();
        while let Some(function_start) = function_iter.next() {
            let function_end = if let Some(next_function) = function_iter.peek() {
                **next_function
            } else {
                self.instructions.last().unwrap().ptr + 1
            };
            let mut alias_nodes = HashSet::new();
            writeln!(output, "  subgraph cluster_{} {{", *function_start)?;
            writeln!(
                output,
                "    label={:?};",
                html_escape(&self.cfg_nodes[function_start].label)
            )?;
            writeln!(output, "    tooltip=lbb_{};", *function_start)?;
            emit_cfg_node(
                output,
                dynamic_analysis,
                self,
                *function_start..function_end,
                &mut alias_nodes,
                *function_start,
            )?;
            for alias_node in alias_nodes.iter() {
                writeln!(
                    output,
                    "    alias_{}_lbb_{} [",
                    *function_start, *alias_node
                )?;
                writeln!(output, "        label=lbb_{:?};", *alias_node)?;
                writeln!(output, "        tooltip=lbb_{:?};", *alias_node)?;
                writeln!(output, "        URL=\"#lbb_{:?}\";", *alias_node)?;
                writeln!(output, "    ];")?;
            }
            writeln!(output, "  }}")?;
        }
        for (function_range, cfg_node_start, cfg_node) in self.iter_cfg_by_function() {
            if cfg_node_start != cfg_node.dominator_parent {
                writeln!(
                    output,
                    "  lbb_{} -> lbb_{} [style=dotted; arrowhead=none];",
                    cfg_node_start, cfg_node.dominator_parent,
                )?;
            }
            let mut edges: BTreeMap<usize, usize> = cfg_node
                .destinations
                .iter()
                .map(|destination| (*destination, 0))
                .collect();
            if let Some(dynamic_analysis) = dynamic_analysis {
                if let Some(recorded_edges) = dynamic_analysis.edges.get(&cfg_node_start) {
                    for (destination, recorded_counter) in recorded_edges.iter() {
                        edges
                            .entry(*destination)
                            .and_modify(|counter| {
                                *counter = *recorded_counter;
                            })
                            .or_insert(*recorded_counter);
                    }
                }
            }
            let counter_sum: usize = edges.values().sum();
            if counter_sum == 0 && !edges.is_empty() {
                writeln!(
                    output,
                    "  lbb_{} -> {{{}}};",
                    cfg_node_start,
                    edges
                        .keys()
                        .map(|destination| format!("lbb_{}", *destination))
                        .collect::<Vec<String>>()
                        .join(" ")
                )?;
            } else if let Some(dynamic_analysis) = dynamic_analysis {
                for (destination, counter) in edges {
                    write!(output, "  lbb_{cfg_node_start} -> ")?;
                    if function_range.contains(&destination) {
                        write!(output, "lbb_{destination}")?;
                    } else {
                        write!(
                            output,
                            "alias_{0}_lbb_{1}",
                            function_range.start, destination
                        )?;
                    }
                    writeln!(
                        output,
                        " [label=\"{}\";color=\"{} 1.0 {}.0\"];",
                        counter,
                        counter as f32 / (dynamic_analysis.edge_counter_max as f32 * 3.0)
                            + 2.0 / 3.0,
                        (counter != 0) as i32,
                    )?;
                }
            }
        }
        writeln!(output, "}}")?;
        Ok(())
    }

    /// Finds the strongly connected components
    ///
    /// Generates a topological order as by-product.
    pub fn control_flow_graph_tarjan(&mut self) {
        if self.cfg_nodes.is_empty() {
            return;
        }
        struct NodeState {
            cfg_node: usize,
            discovery: usize,
            lowlink: usize,
            scc_id: usize,
            is_on_scc_stack: bool,
        }
        let mut nodes = self
            .cfg_nodes
            .iter_mut()
            .enumerate()
            .map(|(v, (key, cfg_node))| {
                cfg_node.topo_index.scc_id = v;
                NodeState {
                    cfg_node: *key,
                    discovery: usize::MAX,
                    lowlink: usize::MAX,
                    scc_id: usize::MAX,
                    is_on_scc_stack: false,
                }
            })
            .collect::<Vec<NodeState>>();
        let mut scc_id = 0;
        let mut scc_stack = Vec::new();
        let mut discovered = 0;
        let mut next_v = 1;
        let mut recursion_stack = vec![(0, 0)];
        'dfs: while let Some((v, edge_index)) = recursion_stack.pop() {
            let node = &mut nodes[v];
            if edge_index == 0 {
                node.discovery = discovered;
                node.lowlink = discovered;
                node.is_on_scc_stack = true;
                scc_stack.push(v);
                discovered += 1;
            }
            let cfg_node = self.cfg_nodes.get(&node.cfg_node).unwrap();
            for j in edge_index..cfg_node.destinations.len() {
                let w = self
                    .cfg_nodes
                    .get(&cfg_node.destinations[j])
                    .unwrap()
                    .topo_index
                    .scc_id;
                if nodes[w].discovery == usize::MAX {
                    recursion_stack.push((v, j + 1));
                    recursion_stack.push((w, 0));
                    continue 'dfs;
                } else if nodes[w].is_on_scc_stack {
                    nodes[v].lowlink = nodes[v].lowlink.min(nodes[w].discovery);
                }
            }
            if nodes[v].discovery == nodes[v].lowlink {
                let mut index_in_scc = 0;
                while let Some(w) = scc_stack.pop() {
                    let node = &mut nodes[w];
                    node.is_on_scc_stack = false;
                    node.scc_id = scc_id;
                    node.discovery = index_in_scc;
                    index_in_scc += 1;
                    if w == v {
                        break;
                    }
                }
                scc_id += 1;
            }
            if let Some((w, _)) = recursion_stack.last() {
                nodes[*w].lowlink = nodes[*w].lowlink.min(nodes[v].lowlink);
            } else {
                loop {
                    if next_v == nodes.len() {
                        break 'dfs;
                    }
                    if nodes[next_v].discovery == usize::MAX {
                        break;
                    }
                    next_v += 1;
                }
                recursion_stack.push((next_v, 0));
                next_v += 1;
            }
        }
        for node in &nodes {
            let cfg_node = self.cfg_nodes.get_mut(&node.cfg_node).unwrap();
            cfg_node.topo_index = TopologicalIndex {
                scc_id: node.scc_id,
                discovery: node.discovery,
            };
        }
        let mut topological_order = self.cfg_nodes.keys().cloned().collect::<Vec<_>>();
        topological_order.sort_by(|a, b| {
            self.cfg_nodes[b]
                .topo_index
                .cmp(&self.cfg_nodes[a].topo_index)
        });
        self.topological_order = topological_order;
        let mut super_root = CfgNode {
            instructions: self.instructions.len()..self.instructions.len(),
            ..CfgNode::default()
        };
        let mut first_node = self.topological_order.first().cloned();
        let mut has_external_source = false;
        for (index, v) in self.topological_order.iter().enumerate() {
            let cfg_node = &self.cfg_nodes[v];
            has_external_source |= cfg_node.sources.iter().any(|source| {
                self.cfg_nodes[source].topo_index.scc_id != cfg_node.topo_index.scc_id
            });
            if self
                .topological_order
                .get(index + 1)
                .map(|next_v| {
                    self.cfg_nodes[next_v].topo_index.scc_id != cfg_node.topo_index.scc_id
                })
                .unwrap_or(true)
            {
                if !has_external_source && first_node != Some(self.super_root) {
                    super_root.destinations.push(first_node.unwrap());
                }
                first_node = self.topological_order.get(index + 1).cloned();
                has_external_source = false;
            }
        }
        for v in super_root.destinations.iter() {
            let cfg_node = self.cfg_nodes.get_mut(v).unwrap();
            cfg_node.sources.push(self.super_root);
            self.functions.entry(*v).or_insert_with(|| {
                let name = format!("function_{}", *v);
                let hash = ebpf::hash_symbol_name(name.as_bytes());
                (hash, name)
            });
        }
        self.cfg_nodes.insert(self.super_root, super_root);
    }

    fn control_flow_graph_dominance_intersect(&self, mut a: usize, mut b: usize) -> usize {
        while a != b {
            match self.cfg_nodes[&a]
                .topo_index
                .cmp(&self.cfg_nodes[&b].topo_index)
            {
                std::cmp::Ordering::Greater => {
                    b = self.cfg_nodes[&b].dominator_parent;
                }
                std::cmp::Ordering::Less => {
                    a = self.cfg_nodes[&a].dominator_parent;
                }
                std::cmp::Ordering::Equal => unreachable!(),
            }
        }
        b
    }

    /// Finds the dominance hierarchy of the control-flow graph
    ///
    /// Uses the Cooper-Harvey-Kennedy algorithm.
    pub fn control_flow_graph_dominance_hierarchy(&mut self) {
        if self.cfg_nodes.is_empty() {
            return;
        }
        self.cfg_nodes
            .get_mut(&self.super_root)
            .unwrap()
            .dominator_parent = self.super_root;
        loop {
            let mut terminate = true;
            for b in self.topological_order.iter() {
                let cfg_node = &self.cfg_nodes[b];
                let mut dominator_parent = usize::MAX;
                for p in cfg_node.sources.iter() {
                    if self.cfg_nodes[p].dominator_parent == usize::MAX {
                        continue;
                    }
                    dominator_parent = if dominator_parent == usize::MAX {
                        *p
                    } else {
                        self.control_flow_graph_dominance_intersect(*p, dominator_parent)
                    };
                }
                if cfg_node.dominator_parent != dominator_parent {
                    let cfg_node = self.cfg_nodes.get_mut(b).unwrap();
                    cfg_node.dominator_parent = dominator_parent;
                    terminate = false;
                }
            }
            if terminate {
                break;
            }
        }
        for b in self.topological_order.iter() {
            let cfg_node = &self.cfg_nodes[b];
            assert_ne!(cfg_node.dominator_parent, usize::MAX);
            if *b == cfg_node.dominator_parent {
                continue;
            }
            let p = cfg_node.dominator_parent;
            let dominator_cfg_node = self.cfg_nodes.get_mut(&p).unwrap();
            dominator_cfg_node.dominated_children.push(*b);
        }
    }

    /// Connect the dependencies between the instructions inside of the basic blocks
    pub fn intra_basic_block_data_flow(&mut self) -> BTreeMap<usize, HashMap<DataResource, usize>> {
        fn bind(
            state: &mut (
                usize,
                BTreeMap<DfgNode, BTreeSet<DfgEdge>>,
                HashMap<DataResource, usize>,
            ),
            insn: &ebpf::Insn,
            is_output: bool,
            resource: DataResource,
        ) {
            let kind = if is_output {
                DfgEdgeKind::Empty
            } else {
                DfgEdgeKind::Filled
            };
            let source = if let Some(source) = state.2.get(&resource) {
                DfgNode::InstructionNode(*source)
            } else {
                DfgNode::PhiNode(state.0)
            };
            let destination = DfgNode::InstructionNode(insn.ptr);
            state.1.entry(source.clone()).or_default().insert(DfgEdge {
                source,
                destination,
                kind,
                resource: resource.clone(),
            });
            if is_output {
                state.2.insert(resource, insn.ptr);
            }
        }
        let mut state = (0, BTreeMap::new(), HashMap::new());
        let data_dependencies = self
            .cfg_nodes
            .iter()
            .map(|(basic_block_start, basic_block)| {
                state.0 = *basic_block_start;
                for insn in self.instructions[basic_block.instructions.clone()].iter() {
                    match insn.opc {
                        ebpf::LD_DW_IMM => {
                            bind(&mut state, insn, true, DataResource::Register(insn.dst));
                        }
                        ebpf::LD_B_REG | ebpf::LD_H_REG | ebpf::LD_W_REG | ebpf::LD_DW_REG => {
                            bind(&mut state, insn, false, DataResource::Memory);
                            bind(&mut state, insn, false, DataResource::Register(insn.src));
                            bind(&mut state, insn, true, DataResource::Register(insn.dst));
                        }
                        ebpf::ST_B_IMM | ebpf::ST_H_IMM | ebpf::ST_W_IMM | ebpf::ST_DW_IMM => {
                            bind(&mut state, insn, false, DataResource::Register(insn.dst));
                            bind(&mut state, insn, true, DataResource::Memory);
                        }
                        ebpf::ST_B_REG | ebpf::ST_H_REG | ebpf::ST_W_REG | ebpf::ST_DW_REG => {
                            bind(&mut state, insn, false, DataResource::Register(insn.src));
                            bind(&mut state, insn, false, DataResource::Register(insn.dst));
                            bind(&mut state, insn, true, DataResource::Memory);
                        }
                        ebpf::ADD32_IMM
                        | ebpf::SUB32_IMM
                        | ebpf::MUL32_IMM
                        | ebpf::DIV32_IMM
                        | ebpf::SDIV32_IMM
                        | ebpf::OR32_IMM
                        | ebpf::AND32_IMM
                        | ebpf::LSH32_IMM
                        | ebpf::RSH32_IMM
                        | ebpf::MOD32_IMM
                        | ebpf::XOR32_IMM
                        | ebpf::ARSH32_IMM
                        | ebpf::ADD64_IMM
                        | ebpf::SUB64_IMM
                        | ebpf::MUL64_IMM
                        | ebpf::DIV64_IMM
                        | ebpf::SDIV64_IMM
                        | ebpf::OR64_IMM
                        | ebpf::AND64_IMM
                        | ebpf::LSH64_IMM
                        | ebpf::RSH64_IMM
                        | ebpf::MOD64_IMM
                        | ebpf::XOR64_IMM
                        | ebpf::ARSH64_IMM
                        | ebpf::HOR64_IMM
                        | ebpf::NEG32
                        | ebpf::NEG64
                        | ebpf::LE
                        | ebpf::BE => {
                            bind(&mut state, insn, false, DataResource::Register(insn.dst));
                            bind(&mut state, insn, true, DataResource::Register(insn.dst));
                        }
                        ebpf::MOV32_IMM | ebpf::MOV64_IMM => {
                            bind(&mut state, insn, true, DataResource::Register(insn.dst));
                        }
                        ebpf::ADD32_REG
                        | ebpf::SUB32_REG
                        | ebpf::MUL32_REG
                        | ebpf::DIV32_REG
                        | ebpf::SDIV32_REG
                        | ebpf::OR32_REG
                        | ebpf::AND32_REG
                        | ebpf::LSH32_REG
                        | ebpf::RSH32_REG
                        | ebpf::MOD32_REG
                        | ebpf::XOR32_REG
                        | ebpf::ARSH32_REG
                        | ebpf::ADD64_REG
                        | ebpf::SUB64_REG
                        | ebpf::MUL64_REG
                        | ebpf::DIV64_REG
                        | ebpf::SDIV64_REG
                        | ebpf::OR64_REG
                        | ebpf::AND64_REG
                        | ebpf::LSH64_REG
                        | ebpf::RSH64_REG
                        | ebpf::MOD64_REG
                        | ebpf::XOR64_REG
                        | ebpf::ARSH64_REG => {
                            bind(&mut state, insn, false, DataResource::Register(insn.src));
                            bind(&mut state, insn, false, DataResource::Register(insn.dst));
                            bind(&mut state, insn, true, DataResource::Register(insn.dst));
                        }
                        ebpf::MOV32_REG | ebpf::MOV64_REG => {
                            bind(&mut state, insn, false, DataResource::Register(insn.src));
                            bind(&mut state, insn, true, DataResource::Register(insn.dst));
                        }
                        ebpf::JEQ_IMM
                        | ebpf::JGT_IMM
                        | ebpf::JGE_IMM
                        | ebpf::JLT_IMM
                        | ebpf::JLE_IMM
                        | ebpf::JSET_IMM
                        | ebpf::JNE_IMM
                        | ebpf::JSGT_IMM
                        | ebpf::JSGE_IMM
                        | ebpf::JSLT_IMM
                        | ebpf::JSLE_IMM => {
                            bind(&mut state, insn, false, DataResource::Register(insn.dst));
                        }
                        ebpf::JEQ_REG
                        | ebpf::JGT_REG
                        | ebpf::JGE_REG
                        | ebpf::JLT_REG
                        | ebpf::JLE_REG
                        | ebpf::JSET_REG
                        | ebpf::JNE_REG
                        | ebpf::JSGT_REG
                        | ebpf::JSGE_REG
                        | ebpf::JSLT_REG
                        | ebpf::JSLE_REG => {
                            bind(&mut state, insn, false, DataResource::Register(insn.src));
                            bind(&mut state, insn, false, DataResource::Register(insn.dst));
                        }
                        ebpf::CALL_REG | ebpf::CALL_IMM => {
                            if insn.opc == ebpf::CALL_REG
                                && !(ebpf::FIRST_SCRATCH_REG
                                    ..ebpf::FIRST_SCRATCH_REG + ebpf::SCRATCH_REGS)
                                    .contains(&(insn.imm as usize))
                            {
                                bind(
                                    &mut state,
                                    insn,
                                    false,
                                    DataResource::Register(insn.imm as u8),
                                );
                            }
                            bind(&mut state, insn, false, DataResource::Memory);
                            bind(&mut state, insn, true, DataResource::Memory);
                            for reg in (0..ebpf::FIRST_SCRATCH_REG).chain([10].iter().cloned()) {
                                bind(&mut state, insn, false, DataResource::Register(reg as u8));
                                bind(&mut state, insn, true, DataResource::Register(reg as u8));
                            }
                        }
                        ebpf::EXIT => {
                            bind(&mut state, insn, false, DataResource::Memory);
                            for reg in (0..ebpf::FIRST_SCRATCH_REG).chain([10].iter().cloned()) {
                                bind(&mut state, insn, false, DataResource::Register(reg as u8));
                            }
                        }
                        _ => {}
                    }
                }
                let mut deps = HashMap::new();
                std::mem::swap(&mut deps, &mut state.2);
                (*basic_block_start, deps)
            })
            .collect();
        self.dfg_forward_edges = state.1;
        data_dependencies
    }

    /// Connect the dependencies inbetween the basic blocks
    pub fn inter_basic_block_data_flow(
        &mut self,
        basic_block_outputs: BTreeMap<usize, HashMap<DataResource, usize>>,
    ) {
        let mut continue_propagation = true;
        while continue_propagation {
            continue_propagation = false;
            for basic_block_start in self.topological_order.iter().rev() {
                if !self
                    .dfg_forward_edges
                    .contains_key(&DfgNode::PhiNode(*basic_block_start))
                {
                    continue;
                }
                let basic_block = &self.cfg_nodes[basic_block_start];
                let mut edges = BTreeSet::new();
                std::mem::swap(
                    self.dfg_forward_edges
                        .get_mut(&DfgNode::PhiNode(*basic_block_start))
                        .unwrap(),
                    &mut edges,
                );
                for predecessor in basic_block.sources.iter() {
                    let provided_outputs = &basic_block_outputs[predecessor];
                    for edge in edges.iter() {
                        let mut source_is_a_phi_node = false;
                        let source = if let Some(source) = provided_outputs.get(&edge.resource) {
                            DfgNode::InstructionNode(*source)
                        } else {
                            source_is_a_phi_node = true;
                            DfgNode::PhiNode(*predecessor)
                        };
                        let mut edge = edge.clone();
                        if basic_block.sources.len() != 1 {
                            edge.destination = DfgNode::PhiNode(*basic_block_start);
                        }
                        if self
                            .dfg_forward_edges
                            .entry(source.clone())
                            .or_default()
                            .insert(edge.clone())
                            && source_is_a_phi_node
                            && source != DfgNode::PhiNode(*basic_block_start)
                        {
                            continue_propagation = true;
                        }
                    }
                }
                let reflective_edges = self
                    .dfg_forward_edges
                    .get_mut(&DfgNode::PhiNode(*basic_block_start))
                    .unwrap();
                for edge in reflective_edges.iter() {
                    if edges.insert(edge.clone()) {
                        continue_propagation = true;
                    }
                }
                std::mem::swap(reflective_edges, &mut edges);
            }
        }
        for (basic_block_start, basic_block) in self.cfg_nodes.iter() {
            if basic_block.sources.len() == 1 {
                self.dfg_forward_edges
                    .remove(&DfgNode::PhiNode(*basic_block_start));
            }
        }
        for dfg_edges in self.dfg_forward_edges.values() {
            for dfg_edge in dfg_edges.iter() {
                self.dfg_reverse_edges
                    .entry(dfg_edge.destination.clone())
                    .or_default()
                    .insert(dfg_edge.clone());
            }
        }
    }
}
