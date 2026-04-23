import ReactMarkdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import { Prism as SyntaxHighlighter } from 'react-syntax-highlighter'
import { oneDark } from 'react-syntax-highlighter/dist/esm/styles/prism'

interface MarkdownProps {
  content: string
  className?: string
}

export default function Markdown({ content, className = '' }: MarkdownProps) {
  return (
    <ReactMarkdown
      className={`prose prose-invert prose-sm max-w-none break-words
        prose-headings:text-gray-800 dark:prose-headings:text-gray-200 prose-headings:font-semibold
        prose-p:text-gray-700 dark:prose-p:text-gray-300 prose-p:leading-relaxed
        prose-a:text-blue-400 prose-a:no-underline hover:prose-a:underline
        prose-strong:text-gray-800 dark:prose-strong:text-gray-200
        prose-code:text-blue-300 prose-code:bg-white dark:prose-code:bg-gray-800 prose-code:px-1 prose-code:py-0.5 prose-code:rounded prose-code:text-xs prose-code:before:content-none prose-code:after:content-none
        prose-pre:bg-transparent prose-pre:p-0
        prose-blockquote:border-gray-300 dark:prose-blockquote:border-gray-700 prose-blockquote:text-gray-600 dark:prose-blockquote:text-gray-400
        prose-li:text-gray-700 dark:prose-li:text-gray-300
        prose-th:text-gray-700 dark:prose-th:text-gray-300 prose-td:text-gray-600 dark:prose-td:text-gray-400
        prose-hr:border-gray-300 dark:prose-hr:border-gray-700
        ${className}`}
      remarkPlugins={[remarkGfm]}
      components={{
        code({ className: codeClassName, children, ...props }) {
          const match = /language-(\w+)/.exec(codeClassName || '')
          const codeString = String(children).replace(/\n$/, '')

          if (match) {
            return (
              <SyntaxHighlighter
                style={oneDark}
                language={match[1]}
                PreTag="div"
                customStyle={{
                  margin: 0,
                  borderRadius: '0.375rem',
                  fontSize: '0.75rem',
                }}
              >
                {codeString}
              </SyntaxHighlighter>
            )
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
      }}
    >
      {content}
    </ReactMarkdown>
  )
}
