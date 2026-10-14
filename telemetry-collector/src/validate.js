// Interprets the subset of JSON Schema 2020-12 used by schema.json, so the
// schema file stays the single contract. Workers forbid runtime code
// generation, which rules out compiling validators such as ajv.

const ANNOTATIONS = new Set(["$schema", "$id", "title", "description", "$defs"]);
const ASSERTIONS = new Set([
  "$ref",
  "type",
  "const",
  "enum",
  "pattern",
  "format",
  "maxLength",
  "minimum",
  "maximum",
  "items",
  "maxItems",
  "properties",
  "required",
  "additionalProperties",
  "propertyNames",
  "maxProperties",
]);
const DATE = /^(\d{4})-(\d{2})-(\d{2})$/;

/**
 * Returns `validate(value)`, which yields `null` for a valid value or a
 * message naming the first offending JSON pointer. Throws if the schema uses
 * a keyword this interpreter does not enforce.
 */
export function compileSchema(root) {
  const patterns = new Map();

  const check = (node, at) => {
    for (const key of Object.keys(node)) {
      if (!ANNOTATIONS.has(key) && !ASSERTIONS.has(key)) {
        throw new Error(`schema ${at}: unsupported keyword ${key}`);
      }
    }
    if (node.$ref !== undefined) resolve(node.$ref);
    for (const type of [node.type ?? []].flat()) hasType(null, type);
    if (node.pattern !== undefined) patterns.set(node.pattern, new RegExp(node.pattern, "u"));
    if (node.format !== undefined && node.format !== "date") {
      throw new Error(`schema ${at}: unsupported format ${node.format}`);
    }
    for (const value of node.enum ?? []) {
      if (value !== null && typeof value === "object") {
        throw new Error(`schema ${at}: enum values must be scalars`);
      }
    }
    for (const [name, sub] of Object.entries(node.properties ?? {})) check(sub, `${at}/properties/${name}`);
    for (const [name, sub] of Object.entries(node.$defs ?? {})) check(sub, `${at}/$defs/${name}`);
    if (typeof node.additionalProperties === "object") check(node.additionalProperties, `${at}/additionalProperties`);
    if (node.propertyNames !== undefined) check(node.propertyNames, `${at}/propertyNames`);
    if (node.items !== undefined) check(node.items, `${at}/items`);
  };

  const resolve = (ref) => {
    const match = /^#\/\$defs\/([A-Za-z0-9_]+)$/.exec(ref);
    const target = match && Object.hasOwn(root.$defs ?? {}, match[1]) ? root.$defs[match[1]] : undefined;
    if (target === undefined) throw new Error(`schema: unresolvable $ref ${ref}`);
    return target;
  };

  const validate = (node, value, path) => {
    const where = path || "/";
    if (node.$ref !== undefined) {
      const error = validate(resolve(node.$ref), value, path);
      if (error) return error;
    }
    if (node.type !== undefined) {
      const types = Array.isArray(node.type) ? node.type : [node.type];
      if (!types.some((type) => hasType(value, type))) return `${where}: expected ${types.join(" or ")}`;
    }
    if (node.const !== undefined && value !== node.const) return `${where}: must be ${JSON.stringify(node.const)}`;
    if (node.enum !== undefined && !node.enum.includes(value)) return `${where}: not an allowed value`;

    if (typeof value === "string") {
      if (node.maxLength !== undefined && [...value].length > node.maxLength) return `${where}: too long`;
      if (node.pattern !== undefined && !patterns.get(node.pattern).test(value)) return `${where}: invalid format`;
      if (node.format === "date" && !isDate(value)) return `${where}: invalid date`;
    }
    if (typeof value === "number") {
      if (node.minimum !== undefined && value < node.minimum) return `${where}: below minimum`;
      if (node.maximum !== undefined && value > node.maximum) return `${where}: above maximum`;
    }
    if (Array.isArray(value)) {
      if (node.maxItems !== undefined && value.length > node.maxItems) return `${where}: too many items`;
      if (node.items !== undefined) {
        for (let i = 0; i < value.length; i++) {
          const error = validate(node.items, value[i], `${path}/${i}`);
          if (error) return error;
        }
      }
    }
    if (isObject(value)) {
      const keys = Object.keys(value);
      const properties = node.properties ?? {};
      if (node.maxProperties !== undefined && keys.length > node.maxProperties) return `${where}: too many fields`;
      for (const key of keys) {
        const at = `${path}/${key}`;
        if (node.propertyNames !== undefined) {
          const error = validate(node.propertyNames, key, at);
          if (error) return error;
        }
        if (Object.hasOwn(properties, key)) {
          const error = validate(properties[key], value[key], at);
          if (error) return error;
        } else if (node.additionalProperties === false) {
          return `${at}: unknown field`;
        } else if (typeof node.additionalProperties === "object") {
          const error = validate(node.additionalProperties, value[key], at);
          if (error) return error;
        }
      }
      for (const key of node.required ?? []) {
        if (!Object.hasOwn(value, key)) return `${path}/${key}: missing field`;
      }
    }
    return null;
  };

  check(root, "#");
  return (value) => validate(root, value, "");
}

function isObject(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function hasType(value, type) {
  switch (type) {
    case "null":
      return value === null;
    case "boolean":
      return typeof value === "boolean";
    case "integer":
      return Number.isInteger(value);
    case "number":
      return typeof value === "number";
    case "string":
      return typeof value === "string";
    case "array":
      return Array.isArray(value);
    case "object":
      return isObject(value);
    default:
      throw new Error(`schema: unsupported type ${type}`);
  }
}

function isDate(value) {
  const match = DATE.exec(value);
  if (!match) return false;
  const date = new Date(Date.UTC(Number(match[1]), Number(match[2]) - 1, Number(match[3])));
  return date.toISOString().slice(0, 10) === value;
}
