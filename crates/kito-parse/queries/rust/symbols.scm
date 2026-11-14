; Symbols of a Rust source file.
;
; Captures understood by kito-parse (the same in every language):
;   @def.<kind>  the whole definition; <kind> is one of module, class, struct,
;                enum, interface, function, method, field, const, var,
;                type_alias, block
;   @name        the node holding the symbol's name
;   @scope       optional prefix of the name (joined with "." unless the
;                pattern sets `scope.joiner`)
;   @sig         nodes that make up the signature (may repeat)
;   @body        the body

(function_item
  name: (identifier) @name
  parameters: (parameters) @sig
  return_type: (_)? @sig
  body: (block) @body) @def.function

(function_signature_item
  name: (identifier) @name
  parameters: (parameters) @sig
  return_type: (_)? @sig) @def.function

(struct_item name: (type_identifier) @name body: (_)? @body) @def.struct
(union_item name: (type_identifier) @name body: (_) @body) @def.struct
(enum_item name: (type_identifier) @name body: (_) @body) @def.enum
(trait_item name: (type_identifier) @name body: (_) @body) @def.interface

; `impl Type` and `impl Trait for Type`: containers of methods.
(impl_item !trait type: (_) @name body: (_)? @body) @def.class
((impl_item trait: (_) @scope type: (_) @name body: (_)? @body) @def.class
  (#set! scope.joiner " for "))

(mod_item name: (identifier) @name body: (_)? @body) @def.module
(const_item name: (identifier) @name) @def.const
(static_item name: (identifier) @name) @def.var
(type_item name: (type_identifier) @name) @def.type_alias
(macro_definition name: (identifier) @name) @def.function
