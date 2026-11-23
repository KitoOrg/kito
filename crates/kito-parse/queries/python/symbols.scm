; Symbols of a Python source file. Capture names: see queries/rust/symbols.scm.

(function_definition
  name: (identifier) @name
  parameters: (parameters) @sig
  return_type: (_)? @sig
  body: (block) @body) @def.function

(class_definition
  name: (identifier) @name
  superclasses: (_)? @sig
  body: (block) @body) @def.class

; The setter and the deleter of a property are defined under its name: what
; their decorator says of them (`@total.setter`) is part of theirs, `setter
; total`. The property itself, its getter, keeps the plain name.
((decorated_definition
  (decorator (attribute object: (identifier) @_property attribute: (identifier) @role))
  definition: (function_definition
    name: (identifier) @name
    parameters: (parameters) @sig
    return_type: (_)? @sig
    body: (block) @body)) @def.function
  (#eq? @_property @name))

; With decorators the definition is the decorated node, so that the
; decorators travel with the symbol.
(decorated_definition
  definition: (function_definition
    name: (identifier) @name
    parameters: (parameters) @sig
    return_type: (_)? @sig
    body: (block) @body)) @def.function

(decorated_definition
  definition: (class_definition
    name: (identifier) @name
    superclasses: (_)? @sig
    body: (block) @body)) @def.class

; Module-level assignments.
(module (expression_statement (assignment left: (identifier) @name) @def.var))
