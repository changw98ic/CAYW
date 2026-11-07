// Pure source mapping and deterministic UTF-8 patch generation for React/Vue
// framework projects. This module never reads files, starts processes, loads
// project configuration or talks to a model. The Rust host owns disk bytes.
//
// Every public offset is a UTF-8 byte offset. Babel and the Vue compilers
// expose JavaScript string indices (UTF-16 code units), so all positions are
// converted at the boundary.

import { createHash } from 'node:crypto';
import * as babelParser from '@babel/parser';
import * as compilerSfc from '@vue/compiler-sfc';
import * as compilerDom from '@vue/compiler-dom';
import MagicString from 'magic-string';

const parseJs = babelParser.parse;
const parseSfc = compilerSfc.parse;
const parseTemplate = compilerDom.parse;
const VueNodeTypes = compilerDom.NodeTypes || {
  ROOT: 0,
  ELEMENT: 1,
  TEXT: 2,
  COMMENT: 3,
  SIMPLE_EXPRESSION: 4,
  INTERPOLATION: 5,
  ATTRIBUTE: 6,
  DIRECTIVE: 7,
};

const MAX_TEXT_BYTES = 256 * 1024;
const MAX_ATTRIBUTE_VALUE_BYTES = 64 * 1024;
const MAX_STYLE_VALUE_BYTES = 512;
const MAX_STYLE_PROPERTIES = 32;

// Tags that never own a closing tag in a template.
const VOID_TAGS = new Set([
  'area', 'base', 'br', 'col', 'embed', 'hr', 'img', 'input', 'link', 'meta',
  'param', 'source', 'track', 'wbr', 'image',
]);

// Non-editable in the HTML engine and here.
const NON_EDITABLE_TAGS = new Set([
  'html', 'head', 'title', 'base', 'meta', 'link', 'script', 'style',
  'template', 'noscript', 'slot', 'iframe', 'frame', 'object', 'embed',
  'canvas', 'video', 'audio', 'source', 'track', 'map', 'area', 'param',
  'col', 'colgroup', 'datalist', 'xmp', 'noembed', 'noframes', 'plaintext',
]);

const NON_INJECT_TAGS = new Set([
  'html', 'head', 'title', 'base', 'meta', 'link', 'script', 'style',
  'template', 'noscript', 'iframe', 'xmp', 'noembed', 'noframes', 'plaintext',
]);

const CONTAINER_TAGS = new Set([
  'body', 'main', 'header', 'footer', 'nav', 'section', 'article', 'aside',
  'div', 'figure', 'figcaption', 'form', 'fieldset', 'ul', 'ol', 'li', 'dl',
  'dt', 'dd', 'table', 'thead', 'tbody', 'tfoot', 'tr', 'td', 'th', 'details',
  'blockquote', 'pre', 'address',
]);

const PROTECTED_STRUCTURE = new Set(['html', 'head', 'body', 'main']);
const TABLE_STRUCTURE_TAGS = new Set(['table', 'thead', 'tbody', 'tfoot', 'tr']);

const RESERVED_ATTRIBUTES = new Set([
  'data-cayw-node', 'data-cayw-readonly', 'data-cayw-reason',
]);

// Native element names recognised in JSX/TSX and Vue templates. Upper-case
// JSX identifiers are components and are never mapped.
const NATIVE_TAGS = new Set([
  'a', 'abbr', 'address', 'area', 'article', 'aside', 'audio', 'b', 'base',
  'bdi', 'bdo', 'blockquote', 'body', 'br', 'button', 'canvas', 'caption',
  'cite', 'code', 'col', 'colgroup', 'data', 'datalist', 'dd', 'del',
  'details', 'dfn', 'dialog', 'div', 'dl', 'dt', 'em', 'embed', 'fieldset',
  'figcaption', 'figure', 'footer', 'form', 'h1', 'h2', 'h3', 'h4', 'h5', 'h6',
  'head', 'header', 'hgroup', 'hr', 'html', 'i', 'iframe', 'img', 'input',
  'ins', 'kbd', 'label', 'legend', 'li', 'link', 'main', 'map', 'mark', 'menu',
  'meta', 'meter', 'nav', 'noscript', 'object', 'ol', 'optgroup', 'option',
  'output', 'p', 'param', 'picture', 'pre', 'progress', 'q', 'rp', 'rt',
  'ruby', 's', 'samp', 'script', 'search', 'section', 'select', 'slot',
  'small', 'source', 'span', 'strong', 'style', 'sub', 'summary', 'sup',
  'table', 'tbody', 'td', 'template', 'textarea', 'tfoot', 'th', 'thead',
  'time', 'title', 'tr', 'track', 'u', 'ul', 'var', 'video', 'wbr',
  // SVG/MathML names, lower-cased for uniform lookup.
  'svg', 'math', 'path', 'circle', 'rect', 'line', 'g', 'defs', 'text',
  'tspan', 'use', 'symbol', 'polygon', 'polyline', 'ellipse',
  'lineargradient', 'radialgradient', 'clippath', 'mask', 'pattern', 'filter',
  'foreignobject', 'marker', 'stop', 'animate', 'animatetransform',
]);

const ATTRIBUTE_NAMES = new Set([
  'href', 'src', 'alt', 'title', 'aria-label', 'aria-labelledby',
  'aria-describedby', 'aria-controls', 'aria-expanded', 'id', 'name',
  'placeholder', 'type', 'target', 'rel', 'download', 'width', 'height',
  'loading', 'decoding', 'role', 'for', 'value', 'action', 'method',
  'autocomplete', 'inputmode', 'pattern', 'min', 'max', 'step', 'disabled',
]);

// HTML attribute name -> React JSX property name.
const JSX_ATTRIBUTE_NAMES = new Map([
  ['class', 'className'],
  ['for', 'htmlFor'],
  ['tabindex', 'tabIndex'],
  ['readonly', 'readOnly'],
  ['maxlength', 'maxLength'],
  ['minlength', 'minLength'],
  ['colspan', 'colSpan'],
  ['rowspan', 'rowSpan'],
  ['autocomplete', 'autoComplete'],
  ['inputmode', 'inputMode'],
  ['autofocus', 'autoFocus'],
  ['crossorigin', 'crossOrigin'],
  ['referrerpolicy', 'referrerPolicy'],
  ['playsinline', 'playsInline'],
  ['fetchpriority', 'fetchPriority'],
  ['hreflang', 'hrefLang'],
  ['datetime', 'dateTime'],
  ['contenteditable', 'contentEditable'],
  ['spellcheck', 'spellCheck'],
  ['srcset', 'srcSet'],
  ['usemap', 'useMap'],
  ['allowfullscreen', 'allowFullScreen'],
  ['formaction', 'formAction'],
  ['formenctype', 'formEncType'],
  ['formmethod', 'formMethod'],
  ['formnovalidate', 'formNoValidate'],
  ['formtarget', 'formTarget'],
  ['enctype', 'encType'],
  ['novalidate', 'noValidate'],
]);

const URL_SAFE_SCHEMES = ['http://', 'https://', 'mailto:', 'tel:'];
const IMAGE_DATA_PREFIXES = [
  'data:image/png', 'data:image/jpeg', 'data:image/gif', 'data:image/webp',
  'data:image/avif', 'data:image/svg+xml',
];

const CONTROL_CHARS = /[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/;

function fail(code, message) {
  const error = new Error(message);
  error.code = code;
  throw error;
}

function sha256(value) {
  return createHash('sha256').update(value, 'utf8').digest('hex');
}

function byteOffset(source, jsIndex) {
  return Buffer.byteLength(source.slice(0, jsIndex), 'utf8');
}

function utf8Length(codePoint) {
  if (codePoint < 0x80) return 1;
  if (codePoint < 0x800) return 2;
  if (codePoint < 0x10000) return 3;
  return 4;
}

function computeLineStarts(source) {
  const starts = [0];
  for (let index = 0; index < source.length; index += 1) {
    if (source.charCodeAt(index) === 10) starts.push(index + 1);
  }
  return starts;
}

function lineIndex(starts, jsIndex) {
  let low = 0;
  let high = starts.length;
  while (low < high) {
    const middle = (low + high) >> 1;
    if (starts[middle] <= jsIndex) low = middle + 1;
    else high = middle;
  }
  return Math.max(low - 1, 0);
}

function nodeId(file, revision, ordinal) {
  return `n${ordinal.toString(16)}-${sha256(`${file}\0${revision}\0${ordinal}`)}`;
}

function assertInput(value, name) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    fail('invalid_input', `${name} must be an object`);
  }
}

function normalizeInput(input) {
  assertInput(input, 'input');
  const framework = input.framework;
  if (framework !== 'react' && framework !== 'vue') {
    fail('invalid_input', 'framework must be react or vue');
  }
  if (typeof input.revision !== 'string' || input.revision.length === 0 || input.revision.length > 256) {
    fail('invalid_input', 'revision must be a non-empty string');
  }
  if (!Array.isArray(input.files) || input.files.length === 0) {
    fail('invalid_input', 'files must be a non-empty array');
  }
  const seen = new Set();
  const files = input.files.map((file) => {
    assertInput(file, 'file');
    if (typeof file.path !== 'string' || file.path.length === 0 || file.path.length > 1024) {
      fail('invalid_input', 'each file needs a relative path');
    }
    if (file.path.startsWith('/') || file.path.split('/').includes('..')) {
      fail('invalid_input', `file path is not relative: ${file.path}`);
    }
    if (typeof file.source !== 'string') {
      fail('invalid_input', `file source must be a string: ${file.path}`);
    }
    if (seen.has(file.path)) {
      fail('invalid_input', `duplicate file path: ${file.path}`);
    }
    seen.add(file.path);
    return { path: file.path, source: file.source };
  });
  return { framework, revision: input.revision, files };
}

function extensionOf(path) {
  const match = /\.([^.\\/]+)$/.exec(path);
  return match ? match[1].toLowerCase() : '';
}

function parserKind(framework, path) {
  const extension = extensionOf(path);
  if (extension === 'vue') return framework === 'vue' ? 'vue' : 'none';
  if (['jsx', 'tsx', 'js', 'mjs', 'cjs', 'ts', 'mts', 'cts'].includes(extension)) {
    return 'react';
  }
  return 'none';
}

function escaper(value) {
  return String(value)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

function escapeText(value) {
  return String(value)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;');
}

function normalizeTag(name) {
  return String(name).toLowerCase();
}

function isNativeComponentTag(name) {
  return /^[A-Z]/.test(name) || name.includes('.') || name.includes(':');
}

// ---------------------------------------------------------------------------
// React / JSX parsing
// ---------------------------------------------------------------------------

function parseJsSource(source, path) {
  const extension = extensionOf(path);
  const plugins = [];
  if (extension === 'ts' || extension === 'mts' || extension === 'cts') {
    plugins.push('typescript');
  } else if (extension === 'tsx') {
    plugins.push('typescript', 'jsx');
  } else {
    plugins.push('jsx');
  }
  plugins.push(
    'classProperties',
    'classPrivateProperties',
    'classPrivateMethods',
    'decorators-legacy',
    'importAttributes',
    'topLevelAwait',
  );
  try {
    return parseJs(source, {
      sourceType: 'unambiguous',
      errorRecovery: false,
      allowReturnOutsideFunction: true,
      allowAwaitOutsideFunction: true,
      plugins,
    });
  } catch (error) {
    fail('parse_failed', `${path}: ${error.message}`);
  }
}

function jsxName(node) {
  if (!node) return '';
  if (node.type === 'JSXIdentifier') return node.name;
  if (node.type === 'JSXNamespacedName') {
    return `${node.namespace.name}:${node.name.name}`;
  }
  return '';
}

function jsxAttributeName(attribute) {
  if (attribute.name) {
    return attribute.name.type === 'JSXIdentifier'
      ? attribute.name.name
      : jsxName(attribute.name);
  }
  return attribute.type === 'JSXIdentifier' ? attribute.name : jsxName(attribute);
}

function reactAttributeValue(source, attribute) {
  const value = attribute.value;
  if (!value) return { hasValue: false, dynamic: false };
  if (value.type === 'StringLiteral') {
    let start = value.start;
    let end = value.end;
    const raw = source.slice(start, end);
    if (raw.length >= 2 && (raw[0] === '"' || raw[0] === "'") && raw[raw.length - 1] === raw[0]) {
      start += 1;
      end -= 1;
    }
    return {
      hasValue: true,
      dynamic: false,
      quoted: true,
      valueStart: start,
      valueEnd: end,
      value: source.slice(start, end),
    };
  }
  return { hasValue: true, dynamic: true, valueStart: value.start, valueEnd: value.end };
}

function reactAttributes(source, opening) {
  const attributes = [];
  let spread = false;
  for (const attribute of opening.attributes) {
    if (attribute.type === 'JSXSpreadAttribute') {
      spread = true;
      continue;
    }
    const name = jsxAttributeName(attribute);
    const lower = name.toLowerCase();
    const info = {
      name,
      lower,
      start: attribute.start,
      end: attribute.end,
      nameStart: attribute.name.start,
      nameEnd: attribute.name.end,
      ...reactAttributeValue(source, attribute),
      node: attribute,
    };
    if (lower === 'style') info.staticStyleObject = jsxStaticStyleAttribute(attribute);
    attributes.push(info);
  }
  return { attributes, spread };
}

function staticStyleKeyName(key) {
  if (!key) return null;
  if (key.type === 'Identifier') return key.name;
  if (key.type === 'StringLiteral') return key.value;
  return null;
}

function staticStyleRawValue(value) {
  if (!value) return null;
  if (value.type === 'StringLiteral') return value.value;
  if (value.type === 'NumericLiteral') return value.value;
  if (value.type === 'UnaryExpression' && value.operator === '-' && value.argument
    && value.argument.type === 'NumericLiteral') {
    return -value.argument.value;
  }
  return null;
}

function staticStyleLiteralValue(value) {
  const raw = staticStyleRawValue(value);
  if (raw === null) return null;
  return typeof raw === 'number' ? String(raw) : raw;
}

// A style object built only from literal values is static source, not a
// computed expression. Structural moves may keep it, and style edits can
// merge into it.
function isStaticStyleObjectExpression(expression) {
  if (!expression || expression.type !== 'ObjectExpression') return false;
  return expression.properties.every((property) => (
    property.type === 'ObjectProperty'
    && !property.computed
    && staticStyleKeyName(property.key) !== null
    && staticStyleRawValue(property.value) !== null
  ));
}

function jsxStaticStyleAttribute(attribute) {
  if (!attribute || jsxAttributeName(attribute).toLowerCase() !== 'style') return false;
  const value = attribute.value;
  return Boolean(value && value.type === 'JSXExpressionContainer'
    && isStaticStyleObjectExpression(value.expression));
}

function cssPropertyFromReactKey(name) {
  if (name.startsWith('--')) return name;
  if (name.startsWith('ms') && name.length > 2 && name[2] === name[2].toUpperCase()) {
    return `-ms-${name.slice(2).replace(/[A-Z]/g, (letter) => `-${letter.toLowerCase()}`)}`;
  }
  return name.replace(/[A-Z]/g, (letter) => `-${letter.toLowerCase()}`);
}

function staticStyleDeclarations(attribute) {
  const expression = attribute.node && attribute.node.value && attribute.node.value.expression;
  const declarations = [];
  for (const property of (expression && expression.properties) || []) {
    const key = staticStyleKeyName(property.key);
    const value = staticStyleRawValue(property.value);
    if (key === null || value === null) continue;
    declarations.push([key.startsWith('--') ? key : cssPropertyFromReactKey(key), value]);
  }
  return declarations;
}

function reactElementInfo(source, node) {
  const opening = node.openingElement;
  const rawName = jsxName(opening.name);
  const tagLower = normalizeTag(rawName);
  const attributes = reactAttributes(source, opening);
  let styleDynamic = false;
  const dynamicAttrNames = new Set();
  let hasHandler = false;
  let hasRef = false;
  let hasId = false;
  let reserved = false;
  let existingNodeAttribute = false;
  for (const attribute of attributes.attributes) {
    if (RESERVED_ATTRIBUTES.has(attribute.lower)) reserved = true;
    if (attribute.lower === 'data-cayw-node') existingNodeAttribute = true;
    if (/^on[a-z]/i.test(attribute.name)) hasHandler = true;
    if (attribute.lower === 'ref') hasRef = true;
    if (attribute.lower === 'id') hasId = true;
    if (attribute.dynamic && !attribute.staticStyleObject) dynamicAttrNames.add(attribute.lower);
    if (attribute.lower === 'style' && attribute.dynamic && !attribute.staticStyleObject) {
      styleDynamic = true;
    }
    if (attribute.lower === 'dangerouslysetinnerhtml') dynamicAttrNames.add('dangerouslysetinnerhtml');
  }
  const children = node.children || [];
  const contentDynamic = attributes.attributes.some((attribute) => (
    attribute.lower === 'dangerouslysetinnerhtml'
  ));
  const textDynamic = contentDynamic
    || !children.every((child) => child.type === 'JSXText');
  return {
    tag: rawName,
    tagLower,
    startJs: node.start,
    endJs: node.end,
    openNameEndJs: opening.name.end,
    contentStartJs: opening.end,
    contentEndJs: node.closingElement ? node.closingElement.start : null,
    selfClosing: !node.closingElement,
    void: VOID_TAGS.has(tagLower),
    attributes,
    styleDynamic,
    dynamicAttrNames,
    textDynamic,
    hasHandler,
    hasRef,
    hasId,
    reserved,
    existingNodeAttribute,
    ast: node,
  };
}

function collectReactRecords(source, ast) {
  const records = [];

  function scanAny(node, context) {
    if (!node || typeof node !== 'object') return;
    if (Array.isArray(node)) {
      for (const child of node) scanAny(child, context);
      return;
    }
    if (node.type === 'JSXElement') {
      scanElement(node, context);
      return;
    }
    if (node.type === 'JSXFragment') {
      for (const child of node.children || []) {
        if (child.type === 'JSXExpressionContainer') {
          scanAny(child.expression, {
            parentRecord: context.parentRecord,
            inheritedDynamic: true,
            foreign: context.foreign,
            foreignRoot: false,
            inChildList: false,
          });
        } else {
          scanAny(child, { ...context, inChildList: true });
        }
      }
      return;
    }
    for (const key of Object.keys(node)) {
      if (['loc', 'start', 'end', 'extra', 'leadingComments', 'trailingComments', 'innerComments', 'comments', 'tokens'].includes(key)) {
        continue;
      }
      const value = node[key];
      if (Array.isArray(value)) {
        for (const child of value) {
          if (child && typeof child === 'object' && typeof child.type === 'string') {
            scanAny(child, context);
          }
        }
      } else if (value && typeof value === 'object' && typeof value.type === 'string') {
        scanAny(value, context);
      }
    }
  }

  function scanElement(node, context) {
    const info = reactElementInfo(source, node);
    const native = NATIVE_TAGS.has(info.tagLower) && !isNativeComponentTag(info.tag);
    const foreign = info.tagLower === 'svg' || info.tagLower === 'math'
      ? true
      : context.foreign;
    let record = null;
    if (native) {
      record = info;
      record.foreign = foreign;
      record.foreignRoot = (info.tagLower === 'svg' || info.tagLower === 'math') && !context.foreign;
      record.inheritedDynamic = context.inheritedDynamic;
      record.parentRecord = context.parentRecord;
      record.inChildList = !!context.inChildList;
      record.framework = 'react';
      record.file = null;
      records.push(record);
    }
    const childParent = native ? record : null;
    const opening = node.openingElement;
    // Attribute expression values may contain JSX; they are dynamic and have
    // no structural parent in the template tree.
    for (const attribute of opening.attributes) {
      if (attribute.type === 'JSXAttribute' && attribute.value && attribute.value.type === 'JSXExpressionContainer') {
        scanAny(attribute.value.expression, {
          parentRecord: null,
          inheritedDynamic: true,
          foreign: false,
          foreignRoot: false,
          inChildList: false,
        });
      }
    }
    for (const child of node.children || []) {
      if (child.type === 'JSXExpressionContainer') {
        scanAny(child.expression, {
          parentRecord: childParent,
          inheritedDynamic: true,
          foreign,
          foreignRoot: false,
          inChildList: false,
        });
      } else {
        scanAny(child, {
          parentRecord: childParent,
          inheritedDynamic: context.inheritedDynamic,
          foreign,
          foreignRoot: false,
          inChildList: true,
        });
      }
    }
  }

  scanAny(ast, {
    parentRecord: null,
    inheritedDynamic: false,
    foreign: false,
    foreignRoot: false,
    inChildList: false,
  });
  return records;
}

function reactReason(record) {
  if (record.reserved) return 'reserved_attribute';
  if (record.foreign) {
    return record.foreignRoot ? 'vector_content' : 'foreign_content';
  }
  if (NON_EDITABLE_TAGS.has(record.tagLower)) return 'non_editable_element';
  if (record.attributes.spread || record.inheritedDynamic) return 'dynamic_region';
  return null;
}

// ---------------------------------------------------------------------------
// Vue parsing
// ---------------------------------------------------------------------------

function vueContentBase(source, template) {
  if (!template) return 0;
  const content = template.content || '';
  if (!content) return template.loc.start.offset;
  const found = source.indexOf(content, template.loc.start.offset);
  return found >= 0 ? found : template.loc.start.offset;
}

function vueAttributeValue(source, base, prop) {
  if (!prop.value) return { hasValue: false, dynamic: false, quoted: false };
  const loc = prop.value.loc;
  let start = base + loc.start.offset;
  let end = base + loc.end.offset;
  const outer = source.slice(start, end);
  let quoted = false;
  if (outer.length >= 2 && (outer[0] === '"' || outer[0] === "'") && outer[outer.length - 1] === outer[0]) {
    quoted = true;
    start += 1;
    end -= 1;
  }
  const content = prop.value.content || '';
  const interpolated = content.includes('{{') || content.includes('}}');
  if (!interpolated && source.slice(start, end) !== content) {
    const found = source.indexOf(content, start);
    if (found >= 0 && found + content.length <= end + 2) {
      start = found;
      end = found + content.length;
    }
  }
  return {
    hasValue: true,
    dynamic: interpolated,
    quoted,
    valueStart: start,
    valueEnd: end,
    value: source.slice(start, end),
  };
}

function vueAttributes(source, base, element) {
  const attributes = [];
  let spread = false;
  let hasHandler = false;
  let hasRef = false;
  let hasId = false;
  let reserved = false;
  let existingNodeAttribute = false;
  let styleDynamic = false;
  let controlDynamic = false;
  let contentDynamic = false;
  const dynamicAttrNames = new Set();
  for (const prop of element.props || []) {
    if (prop.type === VueNodeTypes.DIRECTIVE) {
      const name = prop.name;
      if (name === 'bind') {
        if (!prop.arg) {
          spread = true;
        } else {
          const arg = String(prop.arg.content || '').toLowerCase();
          dynamicAttrNames.add(arg);
          if (arg === 'style') styleDynamic = true;
          if (arg === 'ref') hasRef = true;
        }
      } else if (name === 'on') {
        hasHandler = true;
      } else if (['if', 'else-if', 'else', 'for'].includes(name)) {
        controlDynamic = true;
      } else if (['html', 'text'].includes(name)) {
        contentDynamic = true;
      }
      continue;
    }
    if (prop.type !== VueNodeTypes.ATTRIBUTE) continue;
    const lower = normalizeTag(prop.name);
    const value = vueAttributeValue(source, base, prop);
    if (value.dynamic) dynamicAttrNames.add(lower);
    if (RESERVED_ATTRIBUTES.has(lower)) reserved = true;
    if (lower === 'data-cayw-node') existingNodeAttribute = true;
    if (/^on[a-z]/i.test(prop.name)) hasHandler = true;
    if (lower === 'ref') hasRef = true;
    if (lower === 'id') hasId = true;
    if (lower === 'style' && value.dynamic) styleDynamic = true;
    attributes.push({
      name: prop.name,
      lower,
      start: base + prop.loc.start.offset,
      end: base + prop.loc.end.offset,
      nameStart: base + (prop.nameLoc ? prop.nameLoc.start.offset : prop.loc.start.offset),
      nameEnd: base + (prop.nameLoc ? prop.nameLoc.end.offset : prop.loc.start.offset + prop.name.length),
      ...value,
      node: prop,
    });
  }
  return {
    attributes,
    spread,
    hasHandler,
    hasRef,
    hasId,
    reserved,
    existingNodeAttribute,
    styleDynamic,
    controlDynamic,
    contentDynamic,
    dynamicAttrNames,
  };
}

function vueTagBounds(source, start, end) {
  let index = start + 1;
  let quote = null;
  for (; index < end; index += 1) {
    const character = source[index];
    if (quote) {
      if (character === quote) quote = null;
      continue;
    }
    if (character === '"' || character === "'") {
      quote = character;
      continue;
    }
    if (character === '>') break;
  }
  const openEnd = Math.min(index + 1, end);
  const opening = source.slice(start, openEnd);
  const selfClosing = /\/\s*>$/.test(opening);
  let closeStart = null;
  if (!selfClosing) {
    const found = source.lastIndexOf('</', Math.max(start, end - 2));
    if (found >= openEnd) closeStart = found;
  }
  return { openEnd, closeStart, selfClosing };
}

function collectVueRecords(source, base, templateContent) {
  let root;
  try {
    root = parseTemplate(templateContent);
  } catch (error) {
    fail('parse_failed', error.message);
  }
  const records = [];

  function visitElement(element, parentRecord, inheritedDynamic, foreign) {
    const tag = element.tag;
    const tagLower = normalizeTag(tag);
    const native = NATIVE_TAGS.has(tagLower) && !isNativeComponentTag(tag);
    const elementForeign = tagLower === 'svg' || tagLower === 'math' ? true : foreign;
    const attributes = vueAttributes(source, base, element);
    const bounds = vueTagBounds(source, base + element.loc.start.offset, base + element.loc.end.offset);
    const textDynamic = attributes.contentDynamic
      || !(element.children || []).every((node) => node.type === VueNodeTypes.TEXT);
    let record = null;
    if (native) {
      record = {
        tag,
        tagLower,
        startJs: base + element.loc.start.offset,
        endJs: base + element.loc.end.offset,
        openNameEndJs: base + element.loc.start.offset + 1 + tag.length,
        contentStartJs: bounds.openEnd,
        contentEndJs: bounds.closeStart,
        selfClosing: bounds.selfClosing,
        void: VOID_TAGS.has(tagLower),
        attributes,
        styleDynamic: attributes.styleDynamic,
        dynamicAttrNames: attributes.dynamicAttrNames,
        textDynamic,
        hasHandler: attributes.hasHandler,
        hasRef: attributes.hasRef,
        hasId: attributes.hasId,
        spread: attributes.spread,
        reserved: attributes.reserved,
        controlDynamic: attributes.controlDynamic,
        inheritedDynamic,
        foreign: elementForeign,
        foreignRoot: (tagLower === 'svg' || tagLower === 'math') && !foreign,
        parentRecord,
        framework: 'vue',
        ast: element,
      };
      records.push(record);
    }
    const childParent = native ? record : null;
    const childDynamic = inheritedDynamic || attributes.controlDynamic;
    visitChildren(element, childParent, childDynamic, elementForeign);
  }

  function visitChildren(container, parentRecord, inheritedDynamic, foreign) {
    for (const child of container.children || []) {
      if (child.type === VueNodeTypes.ELEMENT) {
        visitElement(child, parentRecord, inheritedDynamic, foreign);
      } else if (child.type === VueNodeTypes.IF || child.type === VueNodeTypes.FOR) {
        for (const branch of child.branches || [child]) {
          visitChildren(branch, parentRecord, true, foreign);
        }
      }
    }
  }

  visitChildren(root, null, false, false);
  return records;
}

function vueReason(record) {
  if (record.reserved) return 'reserved_attribute';
  if (record.foreign) {
    return record.foreignRoot ? 'vector_content' : 'foreign_content';
  }
  if (NON_EDITABLE_TAGS.has(record.tagLower)) return 'non_editable_element';
  if (record.spread || record.inheritedDynamic || record.controlDynamic) {
    return 'dynamic_region';
  }
  return null;
}

// ---------------------------------------------------------------------------
// File model and instrumentation
// ---------------------------------------------------------------------------

function parseFile(framework, file, revision) {
  const kind = parserKind(framework, file.path);
  const model = {
    path: file.path,
    source: file.source,
    kind,
    records: [],
  };
  if (kind === 'none') return model;
  if (kind === 'react') {
    const ast = parseJsSource(file.source, file.path);
    const records = collectReactRecords(file.source, ast);
    for (const record of records) {
      record.file = file.path;
      record.reason = reactReason(record);
      record.editable = record.reason === null;
      record.inject = !record.existingNodeAttribute
        && !record.foreign
        && !NON_INJECT_TAGS.has(record.tagLower);
      record.spanOk = record.startJs < record.endJs
        && file.source[record.startJs] === '<';
    }
    model.records = records;
  } else if (kind === 'vue') {
    let parsed;
    try {
      parsed = parseSfc(file.source, { filename: file.path });
    } catch (error) {
      fail('parse_failed', `${file.path}: ${error.message}`);
    }
    const errors = parsed.errors || [];
    if (errors.length > 0) {
      fail('parse_failed', `${file.path}: ${errors[0].message || errors[0]}`);
    }
    const template = parsed.descriptor && parsed.descriptor.template;
    if (template) {
      const base = vueContentBase(file.source, template);
      const records = collectVueRecords(file.source, base, template.content || '');
      for (const record of records) {
        record.file = file.path;
        record.reason = vueReason(record);
        record.editable = record.reason === null;
        record.inject = !record.existingNodeAttribute
          && !record.foreign
          && !NON_INJECT_TAGS.has(record.tagLower);
        record.spanOk = record.startJs < record.endJs
          && file.source[record.startJs] === '<';
      }
      model.records = records;
    }
  }
  for (const record of model.records) {
    record.fileSource = file.source;
  }
  model.records.sort((left, right) => left.startJs - right.startJs);
  for (const record of model.records) {
    record.childrenRecords = [];
  }
  for (const record of model.records) {
    if (record.parentRecord && record.parentRecord.childrenRecords) {
      record.parentRecord.childrenRecords.push(record);
    }
  }
  // Convert every JavaScript index to a UTF-8 byte offset in one file pass.
  model.lineStarts = computeLineStarts(file.source);
  let js = 0;
  let bytes = 0;
  for (const record of model.records) {
    while (js < record.startJs) {
      const codePoint = file.source.codePointAt(js);
      if (codePoint === undefined) break;
      bytes += utf8Length(codePoint);
      js += codePoint > 0xffff ? 2 : 1;
    }
    record.startBytes = bytes;
    record.line = lineIndex(model.lineStarts, record.startJs) + 1;
  }
  for (const record of model.records) {
    if (!record.spanOk) {
      record.reason = 'unmapped_source';
      record.editable = false;
      record.inject = false;
    }
  }
  return model;
}

function buildModel(framework, files, revision, instrument) {
  const input = normalizeInput({ framework, files, revision });
  const ordered = [...input.files].sort((left, right) => (
    left.path < right.path ? -1 : left.path > right.path ? 1 : 0
  ));
  const fileModels = ordered.map((file, index) => {
    const model = parseFile(framework, file, revision);
    model.order = index;
    return model;
  });
  let ordinal = 0;
  for (const model of fileModels) {
    for (const record of model.records) {
      record.ordinal = ordinal;
      record.id = nodeId(record.file, revision, ordinal);
      ordinal += 1;
    }
  }
  for (const model of fileModels) {
    for (const record of model.records) {
      record.parentId = record.parentRecord ? record.parentRecord.id : null;
    }
  }
  const nodes = [];
  const byId = new Map();
  for (const model of fileModels) {
    for (const record of model.records) {
      const node = {
        id: record.id,
        tag: record.tag,
        parent: record.parentId,
        line: record.line,
        editable: record.editable,
        reason: record.reason,
        file: model.path,
        start: record.startBytes,
      };
      nodes.push(node);
      byId.set(record.id, { record, model });
    }
  }
  const instrumented = {};
  if (instrument) {
    for (const model of fileModels) {
      if (model.kind === 'none') continue;
      instrumented[model.path] = instrumentFile(model);
    }
  }
  return { nodes, instrumented, byId, files: fileModels };
}

function instrumentFile(model) {
  const magic = new MagicString(model.source);
  let changed = false;
  for (const record of model.records) {
    if (record.reserved) {
      for (const attribute of record.attributes.attributes) {
        if (!RESERVED_ATTRIBUTES.has(attribute.lower)) continue;
        let start = attribute.start;
        if (start > 0 && /\s/.test(model.source[start - 1])) start -= 1;
        magic.remove(start, attribute.end);
        changed = true;
      }
    }
    if (!record.inject || record.reason === 'unmapped_source') continue;
    let injected = ` data-cayw-node="${record.id}"`;
    if (!record.editable) {
      injected += ' data-cayw-readonly="true"';
      if (record.reason) {
        injected += ` data-cayw-reason="${escaper(record.reason)}"`;
      }
    }
    magic.appendLeft(record.openNameEndJs, injected);
    changed = true;
  }
  return changed ? magic.toString() : model.source;
}

// ---------------------------------------------------------------------------
// Public mapping API
// ---------------------------------------------------------------------------

export function mapDocuments(input) {
  assertInput(input, 'input');
  const model = buildModel(input.framework, input.files, input.revision, true);
  return { nodes: model.nodes, instrumented: model.instrumented };
}

// ---------------------------------------------------------------------------
// Edit helpers
// ---------------------------------------------------------------------------

function requireField(operation, name) {
  const value = operation[name];
  if (typeof value !== 'string') {
    fail('invalid_operation', `${name} must be a string`);
  }
  return value;
}

function findRecord(model, id) {
  if (typeof id !== 'string' || id.length > 256) {
    fail('invalid_operation', 'id must be a string');
  }
  const found = model.byId.get(id);
  if (found) return found;
  if (/^n[0-9a-f]+-[0-9a-f]{64}$/.test(id)) {
    fail('stale_id', `the node id ${id} belongs to a different source revision`);
  }
  fail('not_found', `no mapped node has id ${id}`);
}

function ensureEditable(record) {
  if (record.editable) return;
  fail(
    'not_editable',
    `<${record.tag}> at line ${record.line} is read-only: ${record.reason || 'unsupported region'}`,
  );
}

function ensureNotProtected(record) {
  if (PROTECTED_STRUCTURE.has(record.tagLower)) {
    fail('unsupported_target', `<${record.tag}> is protected page structure`);
  }
}

function ensureTableChild(parent, contentTag) {
  if (!TABLE_STRUCTURE_TAGS.has(parent.tagLower)) return;
  let allowed = false;
  if (contentTag) {
    switch (parent.tagLower) {
      case 'table':
        allowed = ['caption', 'colgroup', 'thead', 'tbody', 'tfoot', 'tr'].includes(contentTag);
        break;
      case 'thead':
      case 'tbody':
      case 'tfoot':
        allowed = contentTag === 'tr';
        break;
      case 'tr':
        allowed = contentTag === 'td' || contentTag === 'th';
        break;
      default:
        allowed = true;
    }
  }
  if (allowed) return;
  fail(
    'unsupported_target',
    contentTag
      ? `<${parent.tag}> cannot directly contain <${contentTag}>; select a td, th or another container`
      : `<${parent.tag}> cannot directly receive a generic component; select a td or th`,
  );
}

function ensureNoScript(record) {
  const found = subtreeNodes(record).some((node) => node.tagLower === 'script');
  if (found) {
    fail('unsupported_target', 'an element that contains a script cannot be changed structurally');
  }
}

function containsIdAttribute(record) {
  return subtreeNodes(record).some((node) => node.hasId);
}

function subtreeNodes(record) {
  const result = [];
  const seen = new Set();
  function visit(current) {
    if (!current || seen.has(current)) return;
    seen.add(current);
    result.push(current);
    for (const child of current.childrenRecords || []) visit(child);
  }
  // Child links are filled lazily during model build.
  visit(record);
  return result;
}

function jsxStaticStyleText(node, child) {
  return normalizeTag(jsxName(node.openingElement.name)) === 'style'
    && child.type === 'JSXExpressionContainer'
    && child.expression
    && child.expression.type === 'TemplateLiteral'
    && child.expression.expressions.length === 0;
}

function jsxSubtreeUnsafe(node) {
  if (!node) return false;
  if (node.type === 'JSXElement') {
    const opening = node.openingElement;
    const name = jsxName(opening.name);
    if (isNativeComponentTag(name)) return true;
    if (!NATIVE_TAGS.has(normalizeTag(name))) return true;
    for (const attribute of opening.attributes) {
      if (attribute.type === 'JSXSpreadAttribute') return true;
      const attributeName = jsxAttributeName(attribute);
      if (/^on[a-z]/i.test(attributeName)) return true;
      if (attributeName === 'ref') return true;
      if (attributeName === 'dangerouslySetInnerHTML') return true;
      if (attribute.value && attribute.value.type === 'JSXExpressionContainer') {
        if (jsxStaticStyleAttribute(attribute)) continue;
        return true;
      }
      if (attribute.value && attribute.value.type !== 'StringLiteral') return true;
    }
    for (const child of node.children || []) {
      if (child.type === 'JSXText') continue;
      if (child.type === 'JSXExpressionContainer') {
        if (jsxStaticStyleText(node, child)) continue;
        return true;
      }
      if (child.type === 'JSXSpreadChild') return true;
      if (child.type === 'JSXElement' && jsxSubtreeUnsafe(child)) return true;
      if (child.type === 'JSXFragment') {
        for (const nested of child.children || []) {
          if (nested.type !== 'JSXText') return true;
        }
      }
    }
    return false;
  }
  return true;
}

function vueSubtreeUnsafe(element) {
  if (!element) return false;
  const tag = normalizeTag(element.tag || '');
  if (isNativeComponentTag(element.tag || '') || !NATIVE_TAGS.has(tag)) return true;
  for (const prop of element.props || []) {
    if (prop.type === VueNodeTypes.DIRECTIVE) return true;
    if (prop.type !== VueNodeTypes.ATTRIBUTE) return true;
    const lower = normalizeTag(prop.name);
    if (/^on[a-z]/i.test(prop.name)) return true;
    if (lower === 'ref') return true;
    if (prop.value && typeof prop.value.content === 'string'
      && (prop.value.content.includes('{{') || prop.value.content.includes('}}'))) {
      return true;
    }
  }
  for (const child of element.children || []) {
    if (child.type === VueNodeTypes.TEXT) continue;
    if (child.type === VueNodeTypes.COMMENT) continue;
    if (child.type === VueNodeTypes.INTERPOLATION) return true;
    if (child.type === VueNodeTypes.ELEMENT && vueSubtreeUnsafe(child)) return true;
    if (child.type !== VueNodeTypes.TEXT && child.type !== VueNodeTypes.COMMENT
      && child.type !== VueNodeTypes.ELEMENT) {
      return true;
    }
  }
  return false;
}

function ensureStructurallySafe(record) {
  if (record.framework === 'react') {
    if (jsxSubtreeUnsafe(record.ast)) {
      fail('dynamic_region', 'the element contains handlers, dynamic expressions or components and cannot be changed structurally');
    }
  } else if (vueSubtreeUnsafe(record.ast)) {
    fail('dynamic_region', 'the element contains directives, interpolation or components and cannot be changed structurally');
  }
}

function ensureContainerParent(record) {
  if (!record.parentRecord) {
    fail('unsupported_target', 'the page root has no insertion parent');
  }
  const parent = record.parentRecord;
  if (!parent.editable) {
    fail('not_editable', `the parent <${parent.tag}> is read-only: ${parent.reason || 'unsupported region'}`);
  }
  if (!CONTAINER_TAGS.has(parent.tagLower)) {
    fail('unsupported_target', `<${parent.tag}> cannot receive sibling block content`);
  }
  return parent;
}

function placementOffset(record, placement, contentTag) {
  if (placement === 'before' || placement === 'after') {
    ensureTableChild(ensureContainerParent(record), contentTag);
    return placement === 'before' ? record.startJs : record.endJs;
  }
  if (placement === 'inside') {
    if (!CONTAINER_TAGS.has(record.tagLower)) {
      fail('unsupported_target', `<${record.tag}> is not a supported insertion container`);
    }
    if (record.void || record.selfClosing || record.contentEndJs === null) {
      fail('unsupported_target', 'a void or self-closing element cannot contain inserted content');
    }
    ensureTableChild(record, contentTag);
    return record.contentEndJs;
  }
  fail('invalid_operation', 'placement must be before, after or inside');
}

function findStaticAttribute(record, name) {
  const targetLower = normalizeTag(targetAttributeName(record, name));
  const matches = record.attributes.attributes.filter((attribute) => (
    attribute.lower === name || attribute.lower === targetLower
  ));
  if (matches.length > 1) {
    fail('invalid_operation', `the element has more than one ${name} attribute`);
  }
  return matches[0] || null;
}

function targetAttributeName(record, name) {
  if (record.framework === 'react') {
    return JSX_ATTRIBUTE_NAMES.get(name) || name;
  }
  return name;
}

function currentStyle(record) {
  if (record.styleDynamic) {
    fail('dynamic_region', 'the existing style attribute is dynamic and cannot be overwritten');
  }
  const attribute = findStaticAttribute(record, 'style');
  if (!attribute) return '';
  if (attribute.dynamic) {
    if (record.framework === 'react' && attribute.staticStyleObject) {
      return styleText(staticStyleDeclarations(attribute));
    }
    fail('dynamic_region', 'the existing style attribute is dynamic and cannot be overwritten');
  }
  return attribute.hasValue ? attribute.value : '';
}

function setAttributeEdit(record, name, value) {
  const attributeName = targetAttributeName(record, name);
  const existing = findStaticAttribute(record, name);
  if (existing) {
    if (existing.dynamic) {
      fail('dynamic_region', `the ${name} attribute is dynamic and cannot be changed`);
    }
    if (existing.hasValue) {
      if (record.framework === 'vue' && existing.quoted === false) {
        return {
          jsStart: existing.nameStart,
          jsEnd: existing.valueEnd,
          replacement: `${attributeName}="${escaper(value)}"`,
        };
      }
      return {
        jsStart: existing.valueStart,
        jsEnd: existing.valueEnd,
        replacement: escaper(value),
      };
    }
    return {
      jsStart: existing.nameEnd,
      jsEnd: existing.nameEnd,
      replacement: `="${escaper(value)}"`,
    };
  }
  return {
    jsStart: record.openNameEndJs,
    jsEnd: record.openNameEndJs,
    replacement: ` ${attributeName}="${escaper(value)}"`,
  };
}

// ---------------------------------------------------------------------------
// Style validation (mirrors the HTML engine's allowed subset)
// ---------------------------------------------------------------------------

const STYLE_KINDS = new Map();
for (const [kind, names] of Object.entries({
  color: [
    'color', 'background-color', 'border-color', 'border-top-color',
    'border-right-color', 'border-bottom-color', 'border-left-color',
    'outline-color', 'text-decoration-color', 'caret-color', 'fill', 'stroke',
    'background',
  ],
  length: [
    'width', 'height', 'min-width', 'max-width', 'min-height', 'max-height',
    'top', 'right', 'bottom', 'left', 'inset', 'padding-top', 'padding-right',
    'padding-bottom', 'padding-left', 'padding-inline', 'padding-block',
    'margin-top', 'margin-right', 'margin-bottom', 'margin-left',
    'margin-inline', 'margin-block', 'border-width', 'border-top-width',
    'border-right-width', 'border-bottom-width', 'border-left-width',
    'outline-width', 'row-gap', 'column-gap', 'flex-basis', 'letter-spacing',
    'word-spacing', 'font-size',
  ],
  lengthList: ['padding', 'margin', 'gap', 'border-radius'],
  fontFamily: ['font-family'],
  fontWeight: ['font-weight'],
  lineHeight: ['line-height'],
  textAlign: ['text-align'],
  display: ['display'],
  flexDirection: ['flex-direction'],
  justify: ['justify-content', 'justify-items', 'justify-self'],
  align: ['align-items', 'align-self', 'align-content'],
  grid: [
    'grid-template-columns', 'grid-template-rows', 'grid-auto-columns',
    'grid-auto-rows', 'grid-column', 'grid-row', 'grid-auto-flow',
  ],
  borderStyle: [
    'border-style', 'border-top-style', 'border-right-style',
    'border-bottom-style', 'border-left-style',
  ],
  opacity: ['opacity'],
  overflow: ['overflow', 'overflow-x', 'overflow-y'],
  position: ['position'],
  zIndex: ['z-index'],
  objectFit: ['object-fit'],
  aspectRatio: ['aspect-ratio'],
  number: ['flex-grow', 'flex-shrink'],
  order: ['order'],
  flexWrap: ['flex-wrap'],
  whiteSpace: ['white-space'],
  visibility: ['visibility'],
  textDecoration: ['text-decoration'],
  textTransform: ['text-transform'],
  border: ['border', 'outline', 'font'],
  shadow: ['box-shadow'],
  cursor: ['cursor'],
  transition: ['transition'],
})) {
  for (const name of names) STYLE_KINDS.set(name, kind);
}

function balanced(value) {
  let parentheses = 0;
  let brackets = 0;
  let quote = null;
  let escaped = false;
  for (const character of value) {
    if (quote) {
      if (escaped) {
        escaped = false;
      } else if (character === '\\') {
        escaped = true;
      } else if (character === quote) {
        quote = null;
      }
      continue;
    }
    if (character === '"' || character === "'") quote = character;
    else if (character === '(') parentheses += 1;
    else if (character === ')') parentheses -= 1;
    else if (character === '[') brackets += 1;
    else if (character === ']') brackets -= 1;
    if (parentheses < 0 || brackets < 0) return false;
  }
  return parentheses === 0 && brackets === 0 && quote === null;
}

function safeCssValue(value) {
  const lower = value.toLowerCase();
  if ([...value].some((character) => (
    CONTROL_CHARS.test(character) || [';', '{', '}', '<', '>', '\\', '@', '!', '`'].includes(character)
  ))) {
    return false;
  }
  const blocked = [
    'url(', 'expression(', 'javascript:', 'vbscript:', 'behavior:',
    '-moz-binding', 'image-set(', 'cross-fade(', 'src(', '/*', '*/',
  ];
  return !blocked.some((marker) => lower.includes(marker)) && balanced(value);
}

function splitTopLevel(value, separator) {
  const parts = [];
  let depth = 0;
  let quote = null;
  let escaped = false;
  let start = 0;
  for (let index = 0; index < value.length; index += 1) {
    const character = value[index];
    if (quote) {
      if (escaped) escaped = false;
      else if (character === '\\') escaped = true;
      else if (character === quote) quote = null;
      continue;
    }
    if (character === '"' || character === "'") quote = character;
    else if (character === '(') depth += 1;
    else if (character === ')') depth -= 1;
    if (depth < 0) fail('invalid_style', 'the existing style attribute has unbalanced parentheses');
    if (depth === 0 && character === separator) {
      parts.push(value.slice(start, index));
      start = index + 1;
    }
  }
  if (quote !== null || depth !== 0) {
    fail('invalid_style', 'the existing style attribute is not balanced');
  }
  parts.push(value.slice(start));
  return parts;
}

function splitDeclaration(segment) {
  let depth = 0;
  let quote = null;
  let escaped = false;
  for (let index = 0; index < segment.length; index += 1) {
    const character = segment[index];
    if (quote) {
      if (escaped) escaped = false;
      else if (character === '\\') escaped = true;
      else if (character === quote) quote = null;
      continue;
    }
    if (character === '"' || character === "'") quote = character;
    else if (character === '(') depth += 1;
    else if (character === ')') depth -= 1;
    else if (character === ':' && depth === 0) {
      const name = segment.slice(0, index).trim();
      if (!name) break;
      return [name, segment.slice(index + 1)];
    }
  }
  fail('invalid_style', 'the existing style attribute contains a declaration without a property');
  return ['', ''];
}

function normalizedStyleName(name) {
  return name.startsWith('--') ? name : name.toLowerCase();
}

function mergeStyleDeclarations(current, updates) {
  if ([...current].some((character) => CONTROL_CHARS.test(character))) {
    fail('invalid_style', 'the existing style attribute contains an unsupported control character');
  }
  const declarations = [];
  for (const segment of splitTopLevel(current, ';')) {
    const trimmed = segment.trim();
    if (!trimmed) continue;
    const [name, value] = splitDeclaration(trimmed);
    declarations.push([normalizedStyleName(name), value.trim()]);
  }
  for (const [name, value] of updates) {
    let found = false;
    for (const declaration of declarations) {
      if (declaration[0] === name) {
        declaration[1] = value;
        found = true;
      }
    }
    if (!found) declarations.push([name, value]);
  }
  return declarations;
}

function styleText(declarations) {
  return declarations.map(([name, value]) => `${name}: ${value}`).join('; ');
}

// React requires `style` to be an object, never a CSS string. A JSX string
// attribute parses fine but crashes at runtime with "The style prop expects a
// mapping", so every React path must emit a JS object expression instead.
function reactStyleKey(name) {
  if (name.startsWith('--')) return name;
  if (name.startsWith('-ms-')) {
    return `ms${name.slice(4).replace(/-([a-z])/g, (_, letter) => letter.toUpperCase())}`;
  }
  const body = name.startsWith('-') ? name.slice(1) : name;
  const camel = body
    .replace(/^([a-z])/, (letter) => letter.toUpperCase())
    .replace(/-([a-z])/g, (_, letter) => letter.toUpperCase());
  return name.startsWith('-') ? camel : `${body[0] || ''}${camel.slice(1)}`;
}

function reactStyleLiteral(declarations) {
  if (declarations.length === 0) return '{{}}';
  const entries = declarations.map(([name, value]) => {
    const key = name.startsWith('--') ? name : reactStyleKey(name);
    const emittedKey = /^[A-Za-z_$][A-Za-z0-9_$]*$/.test(key) ? key : JSON.stringify(name);
    const emittedValue = typeof value === 'number' ? String(value) : JSON.stringify(value);
    return `${emittedKey}: ${emittedValue}`;
  });
  return `{{ ${entries.join(', ')} }}`;
}

function mergeReactStyleDeclarations(attribute, updates) {
  const existing = attribute && attribute.staticStyleObject ? staticStyleDeclarations(attribute) : [];
  const declarations = [];
  const updatedKeys = new Set();
  for (const [name, value] of existing) {
    let matched = false;
    for (const [updateName, updateValue] of updates) {
      if (name === updateName) {
        declarations.push([name, updateValue]);
        updatedKeys.add(updateName);
        matched = true;
        break;
      }
    }
    if (!matched) {
      declarations.push([name, value]);
    }
  }
  for (const [updateName, updateValue] of updates) {
    if (!updatedKeys.has(updateName)) {
      declarations.push([updateName, updateValue]);
    }
  }
  return declarations;
}

function componentStyleDeclarations(css) {
  try {
    return mergeStyleDeclarations(css, []);
  } catch (error) {
    if (error && typeof error.code === 'string') {
      fail('invalid_operation', `component style is not valid CSS: ${error.message}`);
    }
    throw error;
  }
}

function reactStyleEdit(record, declarations) {
  const literal = reactStyleLiteral(declarations);
  const existing = findStaticAttribute(record, 'style');
  if (existing) {
    if (existing.dynamic && !existing.staticStyleObject) {
      fail('dynamic_region', 'the existing style attribute is dynamic and cannot be overwritten');
    }
    return {
      jsStart: existing.start,
      jsEnd: existing.end,
      replacement: `style=${literal}`,
    };
  }
  return {
    jsStart: record.openNameEndJs,
    jsEnd: record.openNameEndJs,
    replacement: ` style=${literal}`,
  };
}

function stripImportant(value) {
  const trimmed = value.trimEnd();
  const suffix = '!important';
  if (trimmed.length >= suffix.length) {
    const cut = trimmed.length - suffix.length;
    if (trimmed.slice(cut).toLowerCase() === suffix) {
      return [trimmed.slice(0, cut).trimEnd(), true];
    }
  }
  return [trimmed, false];
}

function validColor(value) {
  const lower = value.toLowerCase();
  if (lower.startsWith('#')) {
    const digits = lower.slice(1);
    return [3, 4, 6, 8].includes(digits.length) && /^[0-9a-f]+$/.test(digits);
  }
  if (lower.startsWith('var(')) {
    return lower.slice(4).startsWith('--') && lower.endsWith(')') && balanced(value);
  }
  for (const prefix of ['rgb(', 'rgba(', 'hsl(', 'hsla(', 'hwb(', 'lab(', 'lch(', 'oklab(', 'oklch(', 'color(']) {
    if (lower.startsWith(prefix)) {
      return lower.endsWith(')') && balanced(value)
        && [...value].every((character) => (
          /[a-z0-9]/i.test(character) || [' ', ',', '.', '%', '/', '(', ')', '+', '-', '_'].includes(character)
        ));
    }
  }
  return lower.length > 0 && lower.length <= 32 && /^[a-z]+$/.test(lower);
}

const LENGTH_UNITS = new Set([
  'px', '%', 'em', 'rem', 'vh', 'vw', 'vmin', 'vmax', 'ch', 'ex', 'lh', 'rlh',
  'vi', 'vb', 'cm', 'mm', 'in', 'pt', 'pc', 'svh', 'svw', 'dvh', 'dvw', 'lvh',
  'lvw', 'cqw', 'cqh', 'cqmin', 'cqmax', 'fr',
]);

function validLength(value) {
  const lower = value.toLowerCase();
  if ([
    'auto', 'none', 'inherit', 'initial', 'unset', 'revert', 'revert-layer',
    'fit-content', 'min-content', 'max-content', 'stretch', 'normal',
  ].includes(lower)) {
    return true;
  }
  if (['calc(', 'clamp(', 'min(', 'max(', 'var('].some((prefix) => lower.startsWith(prefix))) {
    return lower.endsWith(')') && balanced(value)
      && [...value].every((character) => (
        /[a-z0-9]/i.test(character) || [' ', ',', '.', '%', '/', '(', ')', '+', '-', '*'].includes(character)
      ));
  }
  if (lower === '0') return true;
  const match = /^(-?(?:\d+\.?\d*|\.\d+))([a-z%]*)$/.exec(lower);
  return !!match && LENGTH_UNITS.has(match[2]);
}

function splitTopLevelWhitespace(value) {
  const parts = [];
  let depth = 0;
  let start = -1;
  for (let index = 0; index < value.length; index += 1) {
    const character = value[index];
    if (character === '(' || character === '[') depth += 1;
    else if (character === ')' || character === ']') depth -= 1;
    if (depth === 0 && /\s/.test(character)) {
      if (start >= 0) {
        parts.push(value.slice(start, index));
        start = -1;
      }
    } else if (start < 0) {
      start = index;
    }
  }
  if (start >= 0) parts.push(value.slice(start));
  return parts;
}

function validLengthList(value) {
  let parts = splitTopLevelWhitespace(value);
  if (parts.length === 0 || (parts.length > 4 && !value.includes('/'))) return false;
  if (value.includes('/')) {
    parts = value.split('/').flatMap(splitTopLevelWhitespace);
  }
  return parts.length > 0 && parts.every(validLength);
}

function validFontFamily(value) {
  return value.length > 0 && value.length <= 200
    && [...value].every((character) => (
      /[a-z0-9]/i.test(character) || [' ', ',', '-', '_', '.', '"', "'"].includes(character)
    ));
}

function validFontWeight(value) {
  const lower = value.toLowerCase();
  if (['normal', 'bold', 'bolder', 'lighter', 'inherit', 'initial', 'unset'].includes(lower)) {
    return true;
  }
  const number = Number(lower);
  return Number.isInteger(number) && number >= 100 && number <= 900 && number % 100 === 0;
}

function validGrid(value) {
  const lower = value.toLowerCase();
  if (['none', 'auto', 'subgrid', 'masonry', 'inherit', 'initial', 'unset'].includes(lower)) {
    return true;
  }
  return value.trim().length > 0
    && [...value].every((character) => (
      /[a-z0-9]/i.test(character)
      || [' ', '-', '_', '.', '%', ',', '(', ')', '[', ']', '/'].includes(character)
    ))
    && balanced(value);
}

function validAspectRatio(value) {
  if (value.toLowerCase() === 'auto') return true;
  if (!value.includes('/')) return Number(value) > 0;
  const [left, right] = value.split('/');
  return Number(left.trim()) > 0 && Number(right.trim()) > 0;
}

function validShorthand(value) {
  return value.trim().length > 0 && safeCssValue(value);
}

function validateStyleValue(kind, value) {
  const lower = value.toLowerCase();
  let valid = false;
  switch (kind) {
    case 'color': valid = validColor(value); break;
    case 'length': valid = validLength(value); break;
    case 'lengthList': valid = validLengthList(value); break;
    case 'fontFamily': valid = validFontFamily(value); break;
    case 'fontWeight': valid = validFontWeight(value); break;
    case 'lineHeight': valid = validLength(value) || (Number(value) >= 0); break;
    case 'textAlign':
      valid = ['left', 'right', 'center', 'justify', 'start', 'end', 'inherit', 'initial', 'unset'].includes(lower);
      break;
    case 'display':
      valid = [
        'block', 'inline', 'inline-block', 'flex', 'inline-flex', 'grid',
        'inline-grid', 'none', 'contents', 'flow-root', 'table', 'table-row',
        'table-cell', 'list-item', 'inherit', 'initial', 'unset',
      ].includes(lower);
      break;
    case 'flexDirection':
      valid = ['row', 'row-reverse', 'column', 'column-reverse', 'inherit', 'initial', 'unset'].includes(lower);
      break;
    case 'justify':
      valid = [
        'flex-start', 'flex-end', 'center', 'space-between', 'space-around',
        'space-evenly', 'start', 'end', 'stretch', 'normal', 'inherit',
        'initial', 'unset',
      ].includes(lower);
      break;
    case 'align':
      valid = [
        'stretch', 'flex-start', 'flex-end', 'center', 'baseline', 'start',
        'end', 'normal', 'inherit', 'initial', 'unset',
      ].includes(lower);
      break;
    case 'grid': valid = validGrid(value); break;
    case 'borderStyle':
      valid = ['none', 'hidden', 'dotted', 'dashed', 'solid', 'double', 'groove', 'ridge', 'inset', 'outset'].includes(lower);
      break;
    case 'opacity': {
      const number = Number(value);
      valid = Number.isFinite(number) && number >= 0 && number <= 1;
      break;
    }
    case 'overflow':
      valid = ['visible', 'hidden', 'clip', 'scroll', 'auto'].includes(lower);
      break;
    case 'position':
      valid = ['static', 'relative', 'absolute', 'fixed', 'sticky'].includes(lower);
      break;
    case 'zIndex':
      valid = Number.isInteger(Number(value)) || ['auto', 'inherit', 'initial', 'unset'].includes(value);
      break;
    case 'objectFit':
      valid = ['fill', 'contain', 'cover', 'none', 'scale-down'].includes(lower);
      break;
    case 'aspectRatio': valid = validAspectRatio(value); break;
    case 'number': valid = Number(value) >= 0; break;
    case 'order': valid = Number.isInteger(Number(value)); break;
    case 'flexWrap': valid = ['nowrap', 'wrap', 'wrap-reverse'].includes(lower); break;
    case 'whiteSpace':
      valid = ['normal', 'nowrap', 'pre', 'pre-wrap', 'pre-line', 'break-spaces'].includes(lower);
      break;
    case 'visibility': valid = ['visible', 'hidden', 'collapse'].includes(lower); break;
    case 'textDecoration':
      valid = [...value].every((character) => /[a-z0-9]/i.test(character) || ['-', ' '].includes(character));
      break;
    case 'textTransform':
      valid = ['none', 'capitalize', 'uppercase', 'lowercase', 'full-width'].includes(lower);
      break;
    case 'border':
    case 'shadow':
    case 'transition':
      valid = validShorthand(value);
      break;
    case 'cursor':
      valid = [...value].every((character) => /[a-z]/.test(character) || ['-', ' '].includes(character));
      break;
    case 'custom':
      valid = value.trim().length > 0 && safeCssValue(value);
      break;
    default:
      valid = false;
  }
  if (!valid) {
    fail('invalid_style', `unsupported style value ${value}`);
  }
}

function validateStyleProperties(properties) {
  if (!properties || typeof properties !== 'object' || Array.isArray(properties)) {
    fail('invalid_style', 'style properties must be an object');
  }
  const keys = Object.keys(properties);
  if (keys.length === 0) fail('invalid_style', 'style operation requires at least one property');
  if (keys.length > MAX_STYLE_PROPERTIES) {
    fail('invalid_style', `style operation is limited to ${MAX_STYLE_PROPERTIES} properties`);
  }
  const updates = [];
  for (const rawName of keys) {
    const trimmed = rawName.trim();
    const custom = trimmed.startsWith('--');
    const name = custom ? trimmed : trimmed.toLowerCase();
    const validName = custom
      ? name.length > 2 && /^[A-Za-z0-9-]+$/.test(name)
      : name.length > 0 && /^[a-z0-9-]+$/.test(name);
    if (name.length > 64 || !validName) {
      fail('invalid_style', `invalid style property name ${rawName}`);
    }
    const rawValue = properties[rawName];
    if (typeof rawValue !== 'string') {
      fail('invalid_style', `style value for ${name} must be a string`);
    }
    if (Buffer.byteLength(rawValue, 'utf8') > MAX_STYLE_VALUE_BYTES
      || [...rawValue].some((character) => CONTROL_CHARS.test(character))) {
      fail('invalid_style', `style value for ${name} exceeds the limit`);
    }
    const [value, important] = stripImportant(rawValue.trim());
    if (!value || !safeCssValue(value)) {
      fail('invalid_style', `style value for ${name} contains unsupported characters`);
    }
    if (!custom) {
      const kind = STYLE_KINDS.get(name);
      if (!kind) fail('invalid_style', `unsupported style property ${name}`);
      validateStyleValue(kind, value);
    } else {
      validateStyleValue('custom', value);
    }
    updates.push([name, important ? `${value} !important` : value]);
  }
  return updates;
}

// ---------------------------------------------------------------------------
// Attribute validation (mirrors the HTML engine)
// ---------------------------------------------------------------------------

function validateIdentifier(value) {
  if (value.length === 0 || value.length > 128 || !/^[A-Za-z0-9_-]+$/.test(value)) {
    fail('invalid_value', 'identifier must contain 1..128 ASCII letters, digits, hyphens or underscores');
  }
}

function validateUrl(value, allowImageData) {
  if ([...value].some((character) => CONTROL_CHARS.test(character))) {
    fail('invalid_value', 'URL contains a control character');
  }
  const lower = value.trim().toLowerCase();
  if (lower.startsWith('javascript:') || lower.startsWith('vbscript:') || lower.startsWith('file:')) {
    fail('invalid_value', 'this URL scheme is not allowed');
  }
  if (lower.startsWith('data:')) {
    const allowed = allowImageData && IMAGE_DATA_PREFIXES.some((prefix) => lower.startsWith(prefix));
    if (!allowed) fail('invalid_value', 'only image data URLs are allowed for src');
    return;
  }
  if (value === '' || value.startsWith('#') || value.startsWith('/')
    || value.startsWith('./') || value.startsWith('../') || value.startsWith('?')
    || URL_SAFE_SCHEMES.some((prefix) => lower.startsWith(prefix))) {
    return;
  }
  if (value.includes(':')) {
    fail('invalid_value', 'relative URLs must not contain an unknown scheme');
  }
}

function validateAttribute(record, name, value) {
  if (!name || name.length > 64 || !/^[a-z0-9-]+$/.test(name)) {
    fail('invalid_attribute', 'attribute names are limited to lowercase ASCII letters, digits and hyphens');
  }
  if (name.startsWith('on')) {
    fail('invalid_attribute', 'event handler attributes cannot be changed');
  }
  if (name === 'style') {
    fail('invalid_attribute', 'use the style operation for inline styles');
  }
  if (!ATTRIBUTE_NAMES.has(name)) {
    fail('invalid_attribute', `${name} is not an editable attribute`);
  }
  if (Buffer.byteLength(value, 'utf8') > MAX_ATTRIBUTE_VALUE_BYTES
    || [...value].some((character) => CONTROL_CHARS.test(character) || character === '\r')) {
    fail('invalid_value', 'attribute value exceeds the limit or contains a control character');
  }
  const targetLower = normalizeTag(targetAttributeName(record, name));
  if (record.dynamicAttrNames
    && (record.dynamicAttrNames.has(name) || record.dynamicAttrNames.has(targetLower))) {
    fail('dynamic_region', `the ${name} attribute is dynamic and cannot be changed`);
  }
  switch (name) {
    case 'href':
      validateUrl(value, false);
      break;
    case 'src':
      validateUrl(value, true);
      break;
    case 'action':
      validateUrl(value, false);
      break;
    case 'for':
    case 'aria-controls':
    case 'aria-labelledby':
    case 'aria-describedby':
      validateIdentifier(value);
      break;
    case 'target':
      if (!value || (value.startsWith('_') && !['_blank', '_self', '_parent', '_top'].includes(value))) {
        fail('invalid_value', 'target must be a browsing-context name or _blank, _self, _parent or _top');
      }
      break;
    case 'rel':
      if (![...value].every((character) => /[a-z0-9]/i.test(character) || ['-', ' '].includes(character))) {
        fail('invalid_value', 'rel may contain only link-relation tokens');
      }
      break;
    case 'type':
      if (![
        'text', 'button', 'submit', 'reset', 'email', 'password', 'search',
        'number', 'tel', 'url', 'checkbox', 'radio', 'range', 'date', 'time',
        'color',
      ].includes(value)) {
        fail('invalid_value', 'type must be a supported control type');
      }
      break;
    case 'width':
    case 'height':
      if (value !== '' && !/^\d+$/.test(value)) {
        fail('invalid_value', 'width and height must be nonnegative integers');
      }
      break;
    case 'loading':
      if (!['', 'eager', 'lazy'].includes(value)) fail('invalid_value', 'loading must be eager or lazy');
      break;
    case 'decoding':
      if (!['', 'sync', 'async', 'auto'].includes(value)) {
        fail('invalid_value', 'decoding must be sync, async or auto');
      }
      break;
    case 'role':
      if (![...value].every((character) => /[a-z]/.test(character) || ['-', ' '].includes(character))) {
        fail('invalid_value', 'role must contain only role tokens');
      }
      break;
    case 'aria-expanded':
      if (!['', 'true', 'false'].includes(value)) fail('invalid_value', 'aria-expanded must be true or false');
      break;
    case 'method':
      if (!['get', 'post'].includes(value.toLowerCase())) fail('invalid_value', 'method must be get or post');
      break;
    case 'autocomplete':
      if (!/^[A-Za-z0-9-]*$/.test(value)) {
        fail('invalid_value', 'autocomplete must contain only token characters');
      }
      break;
    case 'inputmode':
      if (!['none', 'text', 'decimal', 'numeric', 'tel', 'search', 'email', 'url'].includes(value)) {
        fail('invalid_value', 'inputmode must be a supported input mode');
      }
      break;
    case 'min':
    case 'max':
    case 'step':
      if (!/^[0-9.:%T-]*$/.test(value)) {
        fail('invalid_value', 'min, max and step must be numeric or date tokens');
      }
      break;
    case 'id': {
      validateIdentifier(value);
      break;
    }
    case 'name':
    case 'download':
      if (!/^[A-Za-z0-9_.-]*$/.test(value)) {
        fail('invalid_value', 'name and download must contain only simple token characters');
      }
      break;
    default:
      break;
  }
}

function validateText(text) {
  if (Buffer.byteLength(text, 'utf8') > MAX_TEXT_BYTES) {
    fail('invalid_value', `text exceeds the ${MAX_TEXT_BYTES}-byte limit`);
  }
  if ([...text].some((character) => CONTROL_CHARS.test(character))) {
    fail('invalid_value', 'text contains an unsupported control character');
  }
}

// ---------------------------------------------------------------------------
// Insert snippet conversion
// ---------------------------------------------------------------------------

function unsafeComponentCss(value) {
  const lower = value.toLowerCase();
  return ['url(', 'expression(', 'javascript:', 'vbscript:', 'behavior:', '@import', '-moz-binding']
    .some((marker) => lower.includes(marker));
}

function validateComponentTree(node, path) {
  if (!node || typeof node !== 'object') return;
  if (node.type === VueNodeTypes.ELEMENT) {
    const tag = normalizeTag(node.tag || '');
    if (['script', 'link', 'iframe', 'object', 'embed', 'base', 'meta'].includes(tag)) {
      fail('invalid_operation', `the inserted component contains an unsafe <${tag}> element`);
    }
    if (isNativeComponentTag(node.tag || '') || !NATIVE_TAGS.has(tag)) {
      fail('invalid_operation', `the inserted component contains an unsupported <${node.tag}> element`);
    }
    if (tag === 'style') {
      const css = (node.children || []).map((child) => child.content || '').join('');
      if (unsafeComponentCss(css)) {
        fail('invalid_operation', 'the inserted component contains unsafe CSS');
      }
    }
    for (const prop of node.props || []) {
      if (prop.type === VueNodeTypes.DIRECTIVE) {
        fail('invalid_operation', 'the inserted component contains a directive');
      }
      if (prop.type !== VueNodeTypes.ATTRIBUTE) continue;
      const lower = normalizeTag(prop.name);
      if (lower.startsWith('on')) {
        fail('invalid_operation', 'the inserted component contains an event handler attribute');
      }
      if (lower === 'style' && prop.value && unsafeComponentCss(prop.value.content || '')) {
        fail('invalid_operation', 'the inserted component contains unsafe CSS');
      }
      if ((lower === 'href' || lower === 'src') && prop.value) {
        const value = String(prop.value.content || '').trim().toLowerCase();
        if (value.startsWith('javascript:') || value.startsWith('vbscript:') || value.startsWith('file:')) {
          fail('invalid_operation', 'the inserted component contains an unsafe URL');
        }
      }
    }
  }
  for (const child of node.children || []) {
    validateComponentTree(child, path);
  }
}

function jsxText(value) {
  if (/[{}]/.test(value)) {
    fail('invalid_operation', 'inserted component text contains a JSX expression delimiter');
  }
  return escapeText(value);
}

function serializeJsxNode(node) {
  if (node.type === VueNodeTypes.TEXT) return jsxText(node.content || '');
  if (node.type === VueNodeTypes.COMMENT) return `{/*${node.content || ''}*/}`;
  if (node.type !== VueNodeTypes.ELEMENT) return '';
  const tag = node.tag;
  const lower = normalizeTag(tag);
  const isStyle = lower === 'style';
  const attributes = [];
  for (const prop of node.props || []) {
    const rawName = prop.name;
    const normalized = normalizeTag(rawName);
    const name = JSX_ATTRIBUTE_NAMES.get(normalized) || rawName;
    if (normalized === 'style') {
      const declarations = componentStyleDeclarations(prop.value ? prop.value.content || '' : '');
      attributes.push(`style=${reactStyleLiteral(declarations)}`);
      continue;
    }
    if (!prop.value) {
      attributes.push(name);
      continue;
    }
    attributes.push(`${name}="${escaper(prop.value.content || '')}"`);
  }
  const attributeText = attributes.length > 0 ? ` ${attributes.join(' ')}` : '';
  if (VOID_TAGS.has(lower)) return `<${tag}${attributeText} />`;
  const children = node.children || [];
  if (isStyle) {
    const css = children.map((child) => child.content || '').join('');
    const literal = `\`${css.replace(/\\/g, '\\\\').replace(/`/g, '\\`').replace(/\$\{/g, '\\${')}\``;
    return `<${tag}${attributeText}>{${literal}}</${tag}>`;
  }
  const content = children.map(serializeJsxNode).join('');
  return `<${tag}${attributeText}>${content}</${tag}>`;
}

function serializeComponent(componentHtml, framework) {
  if (typeof componentHtml !== 'string' || componentHtml.trim().length === 0) {
    fail('invalid_operation', 'insert requires a component html string');
  }
  let root;
  try {
    root = parseTemplate(componentHtml);
  } catch (error) {
    fail('invalid_operation', `component html is not valid: ${error.message}`);
  }
  validateComponentTree(root);
  const elements = (root.children || []).filter((child) => child.type === VueNodeTypes.ELEMENT);
  if (elements.length === 0) {
    fail('invalid_operation', 'component html has no element');
  }
  if (framework === 'react') {
    return { snippet: (root.children || []).map(serializeJsxNode).join(''), css: '' };
  }
  // Vue templates must not contain <style> or <script>; @vitejs/plugin-vue
  // refuses to compile them ("Tags with side effect ... are ignored"). Move
  // static component CSS into the SFC style blocks instead.
  const styleNodes = [];
  const collect = (node) => {
    if (!node || typeof node !== 'object') return;
    if (node.type === VueNodeTypes.ELEMENT) {
      if (normalizeTag(node.tag || '') === 'style') {
        styleNodes.push(node);
        return;
      }
    }
    for (const child of node.children || []) collect(child);
  };
  collect(root);
  if (styleNodes.length === 0) return { snippet: componentHtml.trim(), css: '' };
  if (!elements.some((child) => normalizeTag(child.tag || '') !== 'style')) {
    fail('invalid_operation', 'component html has no visible element outside its style block');
  }
  let snippet = componentHtml;
  for (const node of [...styleNodes].sort((left, right) => right.loc.start.offset - left.loc.start.offset)) {
    snippet = snippet.slice(0, node.loc.start.offset) + snippet.slice(node.loc.end.offset);
  }
  const css = styleNodes.map((node) => {
    return (node.children || []).map((child) => {
      if (child.type !== VueNodeTypes.TEXT) {
        fail('invalid_operation', 'component style blocks must contain static CSS text');
      }
      return child.content || '';
    }).join('');
  }).join('\n');
  return { snippet: snippet.trim(), css };
}

function vueStyleAppendEdit(model, css) {
  let parsed;
  try {
    parsed = parseSfc(model.source, { filename: model.path });
  } catch (error) {
    fail('invalid_operation', `cannot add component styles to ${model.path}: ${error.message}`);
  }
  const styles = (parsed.descriptor && parsed.descriptor.styles) || [];
  // Only append to a plain CSS block with no semantic qualifiers. A block with
  // media, scoped, module, src, nonce or any unknown attribute must keep its
  // exact meaning, so leave it untouched and add a new plain block instead.
  const plain = [...styles].reverse().find((style) => {
    const attrs = style.attrs || {};
    if (!Object.keys(attrs).every((key) => key === 'lang')) return false;
    return !attrs.lang || ['css', 'scss', 'less', 'postcss'].includes(String(attrs.lang));
  });
  if (plain) {
    const content = String(plain.content || '');
    const lead = content.length === 0 || content.endsWith('\n') ? '' : '\n';
    const tail = css.endsWith('\n') ? '' : '\n';
    return {
      jsStart: plain.loc.end.offset,
      jsEnd: plain.loc.end.offset,
      replacement: `${lead}${css}${tail}`,
    };
  }
  const prefix = model.source.endsWith('\n') ? '' : '\n';
  return {
    jsStart: model.source.length,
    jsEnd: model.source.length,
    replacement: `${prefix}\n<style>\n${css}\n</style>\n`,
  };
}

// ---------------------------------------------------------------------------
// Patch application and selection
// ---------------------------------------------------------------------------

function patchesFor(model, edits) {
  const sorted = [...edits].sort((left, right) => left.jsStart - right.jsStart || left.jsEnd - right.jsEnd);
  let previousEnd = -1;
  for (const edit of sorted) {
    if (edit.jsStart < 0 || edit.jsEnd < edit.jsStart || edit.jsEnd > model.source.length) {
      fail('unsupported_target', 'the requested source range is not valid');
    }
    if (edit.jsStart < previousEnd) {
      fail('unsupported_target', 'the requested source ranges overlap');
    }
    previousEnd = Math.max(previousEnd, edit.jsEnd);
  }
  const patches = sorted.map((edit) => ({
    path: model.path,
    start: byteOffset(model.source, edit.jsStart),
    end: byteOffset(model.source, edit.jsEnd),
    before: model.source.slice(edit.jsStart, edit.jsEnd),
    replacement: edit.replacement,
  }));
  return { patches, sorted };
}

function sourceAfterEdits(source, edits) {
  const sorted = [...edits].sort((left, right) => right.jsStart - left.jsStart || right.jsEnd - left.jsEnd);
  let output = source;
  for (const edit of sorted) {
    output = output.slice(0, edit.jsStart) + edit.replacement + output.slice(edit.jsEnd);
  }
  return output;
}

function selectionAfter(model, path, hint, mode) {
  if (!hint) return null;
  let matches;
  if (mode === 'exact') {
    matches = model.records.filter((record) => record.editable && record.startJs === hint);
  } else {
    matches = model.records
      .filter((record) => record.editable && record.startJs >= hint)
      .sort((left, right) => left.startJs - right.startJs);
  }
  if (matches.length === 0) return null;
  return { path, start: matches[0].startBytes };
}

// ---------------------------------------------------------------------------
// Public edit API
// ---------------------------------------------------------------------------

export function editDocuments(input) {
  assertInput(input, 'input');
  if (!input.operation || typeof input.operation !== 'object' || Array.isArray(input.operation)) {
    fail('invalid_operation', 'operation must be an object');
  }
  const model = buildModel(input.framework, input.files, input.revision, false);
  const operation = input.operation;
  const kind = operation.kind;
  const allowed = {
    style: ['kind', 'id', 'properties'],
    text: ['kind', 'id', 'text'],
    attribute: ['kind', 'id', 'name', 'value'],
    move: ['kind', 'id', 'target', 'placement'],
    insert: ['kind', 'target', 'placement', 'component'],
    duplicate: ['kind', 'id'],
    delete: ['kind', 'id'],
  };
  if (!allowed[kind]) {
    fail('invalid_operation', `unsupported operation kind ${kind}`);
  }
  for (const key of Object.keys(operation)) {
    if (!allowed[kind].includes(key)) {
      fail('invalid_operation', `the operation contains an unexpected ${key} field`);
    }
  }

  let edits = [];
  let fileModel = null;
  let selectionMode = 'none';
  let selectionHint = null;

  const primary = (id) => {
    const found = findRecord(model, id);
    if (!fileModel) fileModel = found.model;
    else if (found.model !== fileModel) {
      fail('cross_file', 'an operation cannot edit nodes from different files');
    }
    return found.record;
  };

  if (kind === 'style') {
    const record = primary(requireField(operation, 'id'));
    ensureEditable(record);
    const properties = operation.properties;
    const updates = validateStyleProperties(properties);
    if (record.framework === 'react') {
      if (record.styleDynamic) {
        fail('dynamic_region', 'the existing style attribute is dynamic and cannot be overwritten');
      }
      const attribute = findStaticAttribute(record, 'style');
      let declarations;
      if (attribute) {
        if (attribute.dynamic) {
          if (!attribute.staticStyleObject) {
            fail('dynamic_region', 'the existing style attribute is dynamic and cannot be overwritten');
          }
          declarations = mergeReactStyleDeclarations(attribute, updates);
        } else {
          declarations = mergeStyleDeclarations(attribute.hasValue ? attribute.value : '', updates);
        }
      } else {
        declarations = updates;
      }
      if (declarations.some(([, value]) => /!\s*important\s*$/i.test(String(value)))) {
        fail('invalid_style', 'React inline styles cannot express !important; remove it or use a stylesheet class');
      }
      edits = [reactStyleEdit(record, declarations)];
    } else {
      const declarations = mergeStyleDeclarations(currentStyle(record), updates);
      edits = [setAttributeEdit(record, 'style', styleText(declarations))];
    }
    selectionMode = 'exact';
    selectionHint = record.startJs;
  } else if (kind === 'text') {
    const record = primary(requireField(operation, 'id'));
    ensureEditable(record);
    const text = requireField(operation, 'text');
    validateText(text);
    if (record.textDynamic) {
      fail('dynamic_region', 'text editing requires an element whose content is plain static text');
    }
    if (record.void || record.selfClosing || record.contentEndJs === null) {
      fail('unsupported_target', 'text editing requires an element with a closing tag');
    }
    if (record.framework === 'react') {
      if (/[{}]/.test(text)) {
        fail('invalid_value', 'JSX text cannot contain an expression delimiter');
      }
      edits = [{
        jsStart: record.contentStartJs,
        jsEnd: record.contentEndJs,
        replacement: escapeText(text),
      }];
    } else {
      if (text.includes('{{') || text.includes('}}')) {
        fail('invalid_value', 'Vue text cannot contain an interpolation delimiter');
      }
      edits = [{
        jsStart: record.contentStartJs,
        jsEnd: record.contentEndJs,
        replacement: escapeText(text),
      }];
    }
    selectionMode = 'exact';
    selectionHint = record.startJs;
  } else if (kind === 'attribute') {
    const record = primary(requireField(operation, 'id'));
    ensureEditable(record);
    const name = requireField(operation, 'name').toLowerCase();
    const value = requireField(operation, 'value');
    validateAttribute(record, name, value);
    if (name === 'id') {
      for (const { record: candidate } of model.byId.values()) {
        if (candidate === record) continue;
        const existing = candidate.attributes.attributes.find((attribute) => (
          attribute.lower === 'id' && attribute.hasValue && attribute.value === value
        ));
        if (existing) {
          fail('invalid_value', 'the id is already used by another element');
        }
      }
    }
    edits = [setAttributeEdit(record, name, value)];
    selectionMode = 'exact';
    selectionHint = record.startJs;
  } else if (kind === 'insert') {
    const target = primary(requireField(operation, 'target'));
    ensureEditable(target);
    const placement = requireField(operation, 'placement');
    const component = requireField(operation, 'component').toLowerCase();
    const { snippet, css } = serializeComponent(input.componentHtml, target.framework);
    const insertAt = placementOffset(target, placement, null);
    edits = [{ jsStart: insertAt, jsEnd: insertAt, replacement: snippet }];
    selectionMode = 'first';
    selectionHint = insertAt;
    if (css) {
      const styleEdit = vueStyleAppendEdit(fileModel, css);
      edits.push(styleEdit);
      if (styleEdit.jsStart < insertAt) {
        selectionHint = insertAt + styleEdit.replacement.length;
      }
    }
  } else if (kind === 'move') {
    const from = primary(requireField(operation, 'id'));
    const target = primary(requireField(operation, 'target'));
    const placement = requireField(operation, 'placement');
    ensureEditable(from);
    ensureEditable(target);
    ensureNotProtected(from);
    ensureNoScript(from);
    ensureStructurallySafe(from);
    if (PROTECTED_STRUCTURE.has(target.tagLower) && placement !== 'inside') {
      fail('unsupported_target', `<${target.tag}> is protected page structure and only accepts inside placement`);
    }
    if (from === target) {
      fail('unsupported_target', 'an element cannot be moved onto itself');
    }
    const fromStart = from.startJs;
    const fromEnd = from.endJs;
    if (target.startJs > fromStart && target.startJs < fromEnd) {
      fail('unsupported_target', 'an element cannot be moved inside its own subtree');
    }
    const insertAt = placementOffset(target, placement, from.tagLower);
    if (insertAt > fromStart && insertAt < fromEnd) {
      fail('unsupported_target', 'an element cannot be moved inside its own source range');
    }
    const snippet = fileModel.source.slice(fromStart, fromEnd);
    let patch;
    if (insertAt <= fromStart) {
      patch = {
        jsStart: insertAt,
        jsEnd: fromEnd,
        replacement: snippet + fileModel.source.slice(insertAt, fromStart),
      };
      selectionHint = insertAt;
    } else {
      patch = {
        jsStart: fromStart,
        jsEnd: insertAt,
        replacement: fileModel.source.slice(fromEnd, insertAt) + snippet,
      };
      selectionHint = insertAt - (fromEnd - fromStart);
    }
    edits = [patch];
    selectionMode = 'first';
  } else if (kind === 'duplicate') {
    const record = primary(requireField(operation, 'id'));
    ensureEditable(record);
    ensureNotProtected(record);
    ensureNoScript(record);
    ensureStructurallySafe(record);
    if (input.framework === 'react' && !record.inChildList) {
      fail('unsupported_target', 'duplicating a root JSX element would create adjacent elements; select a nested element');
    }
    if (containsIdAttribute(record)) {
      fail('unsupported_target', 'duplicating this element would create duplicate id attributes');
    }
    const snippet = fileModel.source.slice(record.startJs, record.endJs);
    edits = [{ jsStart: record.endJs, jsEnd: record.endJs, replacement: snippet }];
    selectionMode = 'first';
    selectionHint = record.endJs;
  } else if (kind === 'delete') {
    const record = primary(requireField(operation, 'id'));
    ensureEditable(record);
    ensureNotProtected(record);
    ensureNoScript(record);
    ensureStructurallySafe(record);
    edits = [{ jsStart: record.startJs, jsEnd: record.endJs, replacement: '' }];
    selectionMode = 'none';
  }

  const { patches } = patchesFor(fileModel, edits);
  const resultSource = sourceAfterEdits(fileModel.source, edits);
  let resultModel;
  try {
    resultModel = parseFile(
      input.framework,
      { path: fileModel.path, source: resultSource },
      input.revision,
    );
  } catch (error) {
    if (error && error.code === 'parse_failed') {
      fail('parse_failed', `the edited source no longer parses: ${error.message}`);
    }
    throw error;
  }
  const selection = selectionAfter(
    resultModel,
    fileModel.path,
    selectionMode === 'none' ? null : selectionHint,
    selectionMode,
  );
  return { patches, selection };
}
