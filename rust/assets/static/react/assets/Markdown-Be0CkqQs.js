import{r as t,j as o}from"./react-vendor-B7v2HPaI.js";import{S as i,o as u,r as p,g as k,d as b,m as d,s as f,y as g,j as h,b as s,p as c,t as j,a as x,c as S,e as m}from"./syntax-highlighter-BYF-y4SB.js";import{M as w,a as M}from"./markdown-DgJRk3H5.js";const E={javascript:m,js:m,jsx:S,typescript:x,ts:x,tsx:j,python:c,py:c,bash:s,sh:s,shell:s,json:h,yaml:g,yml:g,sql:f,markdown:d,md:d,diff:b,go:k,rust:p,rs:p};for(const[e,r]of Object.entries(E))i.registerLanguage(e,r);const q={margin:0,borderRadius:"0.375rem",fontSize:"0.75rem"},v=t.memo(function({language:r,value:a}){return o.jsx(i,{style:u,language:r,PreTag:"div",customStyle:q,children:a})}),N={code({className:e,children:r,node:a,...l}){const n=/language-(\w+)/.exec(e||""),y=String(r).replace(/\n$/,"");return n?o.jsx(v,{language:n[1],value:y}):o.jsx("code",{className:e,...l,children:r})},pre({children:e}){return o.jsx(o.Fragment,{children:e})}};function O({content:e,className:r=""}){return t.useMemo(()=>o.jsx(w,{className:`prose prose-invert prose-sm max-w-none break-words
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
          ${r}`,remarkPlugins:[M],components:N,children:e}),[e,r])}const D=t.memo(O);export{D as default};
