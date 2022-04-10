use std::{fs::read_to_string, path::Path};

use anyhow::Result;
use rust_lsp::lsp_types::{Location, Position, Range};
use slog_scope::{debug, info, trace};
use tree_sitter::{Node, Parser, Point, Query, QueryCursor, Tree};
use url::Url;

use crate::linemap::LineMap;

macro_rules! find_function_def_str {
    () => {
        r#"
            (
                (function_declarator 
                    (identifier) @function)
                (#match? @function "^{}$")
            )
        "#
    };
}

macro_rules! find_function_refs_str {
    () => {
        r#"
            (
                (call_expression 
                    (identifier) @call)
                (#match? @call "^{}$")
            )
        "#
    };
}

macro_rules! find_variable_def_str {
    () => {
        r#"
            (
                [
                    (init_declarator 
                        (identifier) @variable)
                        
                    (parameter_declaration
                        (identifier) @variable)
                        
                    (declaration
                        (identifier) @variable)
                    
                    (#match? @variable "^{}$")
                ]
            )	        
        "#
    };
}

pub struct ParserContext<'a> {
    source: String,
    tree: Tree,
    linemap: LineMap,
    parser: &'a mut Parser,
}

impl<'a> ParserContext<'a> {
    pub fn new(parser: &'a mut Parser, path: &Path) -> Result<Self> {
        let source = read_to_string(path)?;

        let tree = parser.parse(&source, None).unwrap();

        let linemap = LineMap::new(&source);

        Ok(ParserContext {
            source,
            tree,
            linemap,
            parser,
        })
    }

    pub fn find_definitions(&self, path: &Path, point: Position) -> Result<Option<Vec<Location>>> {
        let current_node = match self.find_node_at_point(point) {
            Some(node) => node,
            None => return Ok(None),
        };

        let parent = match current_node.parent() {
            Some(parent) => parent,
            None => return Ok(None),
        };

        debug!("matching location lookup method for parent-child tuple"; "parent" => parent.kind(), "child" => current_node.kind());

        let locations = match (current_node.kind(), parent.kind()) {
            (_, "call_expression") => {
                let query_str = format!(find_function_def_str!(), current_node.utf8_text(self.source.as_bytes())?);
                self.simple_global_search(path, &query_str)?
            }
            ("identifier", "argument_list") | 
            ("identifier", "field_expression") | 
            ("identifier", "binary_expression") |
            ("identifier", "assignment_expression") => {
                self.tree_climbing_search(path, current_node)?
            }
            _ => return Ok(None),
        };

        info!("finished searching for definitions"; "count" => locations.len(), "definitions" => format!("{:?}", locations));

        Ok(Some(locations))
    }

    pub fn find_references(&self, path: &Path, point: Position) -> Result<Option<Vec<Location>>> {
        let current_node = match self.find_node_at_point(point) {
            Some(node) => node,
            None => return Ok(None),
        };

        let parent = match current_node.parent() {
            Some(parent) => parent,
            None => return Ok(None),
        };

        let locations = match (current_node.kind(), parent.kind()) {
            (_, "function_declarator") => {
                let query_str = format!(find_function_refs_str!(), current_node.utf8_text(self.source.as_bytes())?);
                self.simple_global_search(path, &query_str)?
            }
            _ => return Ok(None),
        };

        info!("finished searching for references"; "count" => locations.len(), "references" => format!("{:?}", locations));

        Ok(Some(locations))
    }

    fn tree_climbing_search(&self, path: &Path, start_node: Node) -> Result<Vec<Location>> {
        let mut locations = vec![];

        let node_text = start_node.utf8_text(self.source.as_bytes())?;

        let query_str = format!(find_variable_def_str!(), node_text);

        debug!("built query string"; "query" => &query_str);

        let mut parent = start_node.parent();

        loop {
            if parent.is_none() {
                trace!("no more parent left, found nothing");
                break;
            }

            let query = Query::new(tree_sitter_glsl::language(), &query_str)?;
            let mut query_cursor = QueryCursor::new();

            trace!("running tree-sitter query for node"; "node" => format!("{:?}", parent.unwrap()), "node_text" => parent.unwrap().utf8_text(self.source.as_bytes()).unwrap());

            for m in query_cursor.matches(&query, parent.unwrap(), self.source.as_bytes()) {
                for capture in m.captures {
                    let start = capture.node.start_position();
                    let end = capture.node.end_position();

                    locations.push(Location {
                        uri: Url::from_file_path(path).unwrap(),
                        range: Range {
                            start: Position {
                                line: start.row as u32,
                                character: start.column as u32,
                            },
                            end: Position {
                                line: end.row as u32,
                                character: end.column as u32,
                            },
                        },
                    });
                }
            }

            if !locations.is_empty() {
                break;
            }

            parent = parent.unwrap().parent();
        }

        Ok(locations)
    }

    fn simple_global_search(&self, path: &Path, query_str: &str) -> Result<Vec<Location>> {
        let query = Query::new(tree_sitter_glsl::language(), query_str)?;
        let mut query_cursor = QueryCursor::new();

        let mut locations = vec![];

        for m in query_cursor.matches(&query, self.root_node(), self.source.as_bytes()) {
            for capture in m.captures {
                let start = capture.node.start_position();
                let end = capture.node.end_position();

                locations.push(Location {
                    uri: Url::from_file_path(path).unwrap(),
                    range: Range {
                        start: Position {
                            line: start.row as u32,
                            character: start.column as u32,
                        },
                        end: Position {
                            line: end.row as u32,
                            character: end.column as u32,
                        },
                    },
                });
            }
        }

        Ok(locations)
    }

    fn root_node(&self) -> Node {
        self.tree.root_node()
    }

    fn find_node_at_point(&self, pos: Position) -> Option<Node> {
        // if we're at the end of an ident, we need to look _back_ one char instead
        // for tree-sitter to find the right node.
        let look_behind = {
            let offset = self.linemap.offset_for_position(pos);
            let char_at = self.source.as_bytes()[offset];
            trace!("looking for non-alpha for point adjustment";
                "offset" => offset, 
                "char" => char_at as char,
                "point" => format!("{:?}", pos),
                "look_behind" => !char_at.is_ascii_alphabetic());
            !char_at.is_ascii_alphabetic()
        };

        let mut start = Point {
            row: pos.line as usize,
            column: pos.character as usize,
        };
        let mut end = Point {
            row: pos.line as usize,
            column: pos.character as usize,
        };

        if look_behind {
            start.column -= 1;
        } else {
            end.column += 1;
        }

        match self.root_node().named_descendant_for_point_range(start, end) {
            Some(node) => {
                debug!("found a node"; 
                    "node" => format!("{:?}", node),
                    "text" => node.utf8_text(self.source.as_bytes()).unwrap(),
                    "start" => format!("{}", start),
                    "end" => format!("{}", end));
                Some(node)
            }
            None => None,
        }
    }
}