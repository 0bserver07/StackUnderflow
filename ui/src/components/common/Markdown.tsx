import { memo, useMemo } from 'react'
import ReactMarkdown, { type Components } from 'react-markdown'
import remarkGfm from 'remark-gfm'
import { PrismLight as SyntaxHighlighter } from 'react-syntax-highlighter'
import { oneDark } from 'react-syntax-highlighter/dist/esm/styles/prism'
import bash from 'react-syntax-highlighter/dist/esm/languages/prism/bash'
import diff from 'react-syntax-highlighter/dist/esm/languages/prism/diff'
import go from 'react-syntax-highlighter/dist/esm/languages/prism/go'
import javascript from 'react-syntax-highlighter/dist/esm/languages/prism/javascript'
import json from 'react-syntax-highlighter/dist/esm/languages/prism/json'
import jsx from 'react-syntax-highlighter/dist/esm/languages/prism/jsx'
import markdown from 'react-syntax-highlighter/dist/esm/languages/prism/markdown'
import python from 'react-syntax-highlighter/dist/esm/languages/prism/python'
import rust from 'react-syntax-highlighter/dist/esm/languages/prism/rust'
import sql from 'react-syntax-highlighter/dist/esm/languages/prism/sql'
import tsx from 'react-syntax-highlighter/dist/esm/languages/prism/tsx'
import typescript from 'react-syntax-highlighter/dist/esm/languages/prism/typescript'
import yaml from 'react-syntax-highlighter/dist/esm/languages/prism/yaml'

// PrismLight ships zero grammars by default; we register only the languages
// that actually show up in coding transcripts. This keeps ~330 unused Prism
// grammars (hundreds of KB) out of the entry bundle. Unregistered languages
// degrade to plain text inside react-syntax-highlighter (the refractor highlight
// call throws "Unknown language" and is caught -> rendered as-is, no crash).
const LANGUAGES: Record<string, typeof javascript> = {
  javascript,
  js: javascript,
  jsx,
  typescript,
  ts: typescript,
  tsx,
  python,
  py: python,
  bash,
  sh: bash,
  shell: bash,
  json,
  yaml,
  yml: yaml,
  sql,
  markdown,
  md: markdown,
  diff,
  go,
  rust,
  rs: rust,
}

for (const [name, grammar] of Object.entries(LANGUAGES)) {
  SyntaxHighlighter.registerLanguage(name, grammar)
}

const CODE_STYLE = {
  margin: 0,
  borderRadius: '0.375rem',
  fontSize: '0.75rem',
} as const

// Memoized per (language, code) so a parent re-render (e.g. a streaming message
// list) never re-tokenizes an unchanged code block.
const CodeBlock = memo(function CodeBlock({
  language,
  value,
}: {
  language?: string
  value: string
}) {
  return (
    <SyntaxHighlighter
      style={oneDark}
      language={language}
      PreTag="div"
      customStyle={CODE_STYLE}
    >
      {value}
    </SyntaxHighlighter>
  )
})

// Stable module-level components object so ReactMarkdown isn't handed a fresh
// reference on every render.
const MARKDOWN_COMPONENTS: Components = {
  code({ className: codeClassName, children, node: _node, ...props }) {
    const match = /language-(\w+)/.exec(codeClassName || '')
    const codeString = String(children).replace(/\n$/, '')

    if (match) {
      return <CodeBlock language={match[1]} value={codeString} />
    }

    return (
      <code className={codeClassName} {...props}>
        {children}
      </code>
    )
  },
  pre({ children }) {
    return <>{children}</>
  },
}

interface MarkdownProps {
  content: string
  className?: string
}

function Markdown({ content, className = '' }: MarkdownProps) {
  // Key the rendered output on the inputs so re-renders that don't change the
  // content (the common case while scrolling/streaming) reuse the already
  // parsed + tokenized tree instead of re-running remark + Prism.
  return useMemo(
    () => (
      <ReactMarkdown
        className={`prose prose-invert prose-sm max-w-none break-words
          prose-headings:text-gray-800 dark:prose-headings:text-gray-200 prose-headings:font-semibold
          prose-p:text-gray-700 dark:prose-p:text-gray-300 prose-p:leading-relaxed
          prose-a:text-blue-400 prose-a:no-underline hover:prose-a:underline
          prose-strong:text-gray-800 dark:prose-strong:text-gray-200
          prose-code:text-blue-700 dark:prose-code:text-blue-300 prose-code:bg-gray-100 dark:prose-code:bg-gray-800 prose-code:px-1 prose-code:py-0.5 prose-code:rounded prose-code:text-xs prose-code:before:content-none prose-code:after:content-none
          prose-pre:bg-transparent prose-pre:p-0
          prose-blockquote:border-gray-300 dark:prose-blockquote:border-gray-700 prose-blockquote:text-gray-600 dark:prose-blockquote:text-gray-400
          prose-li:text-gray-700 dark:prose-li:text-gray-300
          prose-th:text-gray-700 dark:prose-th:text-gray-300 prose-td:text-gray-600 dark:prose-td:text-gray-400
          prose-hr:border-gray-300 dark:prose-hr:border-gray-700
          ${className}`}
        remarkPlugins={[remarkGfm]}
        components={MARKDOWN_COMPONENTS}
      >
        {content}
      </ReactMarkdown>
    ),
    [content, className],
  )
}

// Memo so sibling messages in a list don't re-render (and re-tokenize) when an
// unrelated message updates.
export default memo(Markdown)
