; Symbols of a PHP source file. Capture names: see queries/rust/symbols.scm.

(function_definition
  name: (name) @name
  parameters: (formal_parameters) @sig
  return_type: (_)? @sig
  body: (compound_statement) @body) @def.function

(class_declaration name: (name) @name body: (declaration_list) @body) @def.class
(interface_declaration name: (name) @name body: (declaration_list) @body) @def.interface
(trait_declaration name: (name) @name body: (declaration_list) @body) @def.interface
(enum_declaration name: (name) @name body: (enum_declaration_list) @body) @def.enum

(method_declaration
  name: (name) @name
  parameters: (formal_parameters) @sig
  return_type: (_)? @sig
  body: (compound_statement)? @body) @def.method

(namespace_definition name: (namespace_name) @name body: (compound_statement) @body) @def.module
(const_declaration (const_element (name) @name)) @def.const
