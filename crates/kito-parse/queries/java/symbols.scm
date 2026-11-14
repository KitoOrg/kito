; Symbols of a Java source file. Capture names: see queries/rust/symbols.scm.

(class_declaration name: (identifier) @name body: (class_body) @body) @def.class
(interface_declaration name: (identifier) @name body: (interface_body) @body) @def.interface
(enum_declaration name: (identifier) @name body: (enum_body) @body) @def.enum
(annotation_type_declaration name: (identifier) @name body: (_) @body) @def.interface

(record_declaration
  name: (identifier) @name
  parameters: (formal_parameters) @sig
  body: (class_body) @body) @def.class

(method_declaration
  type: (_) @sig
  name: (identifier) @name
  parameters: (formal_parameters) @sig
  body: (block)? @body) @def.method

(constructor_declaration
  name: (identifier) @name
  parameters: (formal_parameters) @sig
  body: (constructor_body) @body) @def.method
