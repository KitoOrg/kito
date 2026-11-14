; Symbols of a C# source file. Capture names: see queries/rust/symbols.scm.

(namespace_declaration name: (_) @name body: (declaration_list) @body) @def.module
(file_scoped_namespace_declaration name: (_) @name) @def.module

(class_declaration name: (identifier) @name body: (declaration_list)? @body) @def.class
(record_declaration name: (identifier) @name body: (declaration_list)? @body) @def.class
(struct_declaration name: (identifier) @name body: (declaration_list)? @body) @def.struct
(interface_declaration name: (identifier) @name body: (declaration_list)? @body) @def.interface
(enum_declaration name: (identifier) @name body: (enum_member_declaration_list)? @body) @def.enum

(method_declaration
  returns: (_) @sig
  name: (identifier) @name
  parameters: (parameter_list) @sig
  body: (_)? @body) @def.method

(constructor_declaration
  name: (identifier) @name
  parameters: (parameter_list) @sig
  body: (_)? @body) @def.method

(local_function_statement
  name: (identifier) @name
  parameters: (parameter_list) @sig
  body: (_)? @body) @def.function
