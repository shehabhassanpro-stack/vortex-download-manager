import js from "@eslint/js";
import tseslint from "typescript-eslint";

/**
 * Custom RTL-First ESLint Plugin (Engineering-Standards.md §3.3 / P0-T04).
 * Bans physical horizontal CSS utility classes (`ml-*`, `mr-*`, `pl-*`, `pr-*`,
 * `left-*`, `right-*`, `text-left`, `text-right`, `border-l`, `border-r`,
 * `rounded-l-*`, `rounded-r-*`) and inline physical style properties in favor
 * of CSS Logical Properties (`ms-*`, `me-*`, `ps-*`, `pe-*`, `start-*`, `end-*`,
 * `text-start`, `text-end`, `border-s`, `border-e`, `rounded-s-*`, `rounded-e-*`).
 */
const PHYSICAL_CLASS_RE =
  /(?:^|[\s:'"`])(?:[a-z0-9-]+:)*-?(?:ml|mr|pl|pr|left|right|border-l|border-r|rounded-l|rounded-r|rounded-tl|rounded-tr|rounded-bl|rounded-br|inset-l|inset-r|scroll-ml|scroll-mr|scroll-pl|scroll-pr)-[a-z0-9[\]./%-]+|(?:^|[\s:'"`])(?:[a-z0-9-]+:)*(?:text-left|text-right|float-left|float-right|clear-left|clear-right|border-l|border-r)(?=$|[\s'"`])/g;

const PHYSICAL_STYLE_PROPS = new Set([
  "marginLeft",
  "marginRight",
  "paddingLeft",
  "paddingRight",
  "borderLeft",
  "borderRight",
  "borderLeftWidth",
  "borderRightWidth",
  "borderLeftColor",
  "borderRightColor",
  "left",
  "right",
]);

const vortexRtlPlugin = {
  rules: {
    "no-physical-directions": {
      meta: {
        type: "problem",
        docs: {
          description:
            "Enforce CSS Logical Properties instead of physical left/right utilities and inline styles for first-class Arabic RTL support.",
        },
        schema: [],
        messages: {
          physicalClass:
            "Avoid physical horizontal class '{{token}}'. Use CSS Logical Properties (ms-*, me-*, ps-*, pe-*, start-*, end-*, text-start, text-end, border-s, rounded-s-*) instead.",
          physicalStyle:
            "Avoid physical CSS property '{{prop}}'. Use logical inlineStart/inlineEnd equivalents instead.",
        },
      },
      create(context) {
        function inspectClassString(node, value) {
          if (typeof value !== "string") return;
          PHYSICAL_CLASS_RE.lastIndex = 0;
          let match;
          while ((match = PHYSICAL_CLASS_RE.exec(value)) !== null) {
            const token = match[0].trim().replace(/^['"`]+|['"`]+$/g, "");
            context.report({
              node,
              messageId: "physicalClass",
              data: { token },
            });
          }
        }

        return {
          JSXAttribute(node) {
            if (node.name?.name !== "className" && node.name?.name !== "class") return;
            if (node.value?.type === "Literal" && typeof node.value.value === "string") {
              inspectClassString(node.value, node.value.value);
            } else if (
              node.value?.type === "JSXExpressionContainer" &&
              node.value.expression?.type === "TemplateLiteral"
            ) {
              for (const q of node.value.expression.quasis) {
                inspectClassString(q, q.value.raw);
              }
            } else if (
              node.value?.type === "JSXExpressionContainer" &&
              node.value.expression?.type === "Literal" &&
              typeof node.value.expression.value === "string"
            ) {
              inspectClassString(node.value.expression, node.value.expression.value);
            }
          },
          Property(node) {
            if (
              node.key?.type === "Identifier" &&
              PHYSICAL_STYLE_PROPS.has(node.key.name) &&
              node.parent?.type === "ObjectExpression"
            ) {
              // Report physical style properties inside JSX style objects or CSSProperties maps
              let cur = node.parent;
              let insideStyle = false;
              while (cur) {
                if (cur.type === "JSXAttribute" && cur.name?.name === "style") {
                  insideStyle = true;
                  break;
                }
                cur = cur.parent;
              }
              if (insideStyle) {
                context.report({
                  node: node.key,
                  messageId: "physicalStyle",
                  data: { prop: node.key.name },
                });
              }
            }
          },
        };
      },
    },
  },
};

export default tseslint.config(
  {
    ignores: [
      "**/node_modules/**",
      "**/dist/**",
      "**/build/**",
      "**/.output/**",
      "**/.wxt/**",
      "**/target/**",
      "**/storybook-static/**",
      "**/coverage/**",
      "**/src/bindings/generated.ts",
    ],
  },
  js.configs.recommended,
  ...tseslint.configs.strict,
  {
    files: ["**/*.{ts,tsx,mts,cts}"],
    plugins: {
      "vortex-rtl": vortexRtlPlugin,
    },
    rules: {
      "@typescript-eslint/no-explicit-any": "error",
      "@typescript-eslint/ban-ts-comment": [
        "error",
        {
          "ts-ignore": true,
          "ts-nocheck": true,
          "ts-expect-error": "allow-with-description",
          minimumDescriptionLength: 10,
        },
      ],
      "no-console": ["warn", { allow: ["warn", "error"] }],
      "vortex-rtl/no-physical-directions": "error",
      "no-restricted-imports": [
        "error",
        {
          paths: [
            {
              name: "@tauri-apps/api/core",
              importNames: ["invoke"],
              message:
                "Raw Tauri invoke() is prohibited in UI components (Engineering-Standards.md §1.2). Use auto-generated typed functions in src/bindings/ instead.",
            },
          ],
        },
      ],
    },
  },
  {
    files: ["apps/desktop/ui/src/shared/**/*.{ts,tsx}"],
    rules: {
      "no-restricted-imports": [
        "error",
        {
          patterns: [
            {
              group: ["**/features/**", "../features/**", "**/app/**", "../app/**"],
              message:
                "Feature-Sliced Design invariant (Engineering-Standards.md §1.2): shared/ must never import from features/ or app/.",
            },
          ],
        },
      ],
    },
  },
  {
    files: ["apps/desktop/ui/src/bindings/**/*.{ts,tsx}"],
    rules: {
      "no-restricted-imports": "off",
    },
  },
);
