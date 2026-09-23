// A small Markdown renderer for Hive and agent replies: fenced code, headings,
// lists, quotes, and inline code, bold, italics and links. It builds React
// elements directly, so model text never reaches the page as HTML.
import { Fragment, ReactNode } from "react";

// Emphasis needs a non-word character outside each delimiter, so identifiers
// like tool_use_id and globs like *.ts and *.rs stay literal.
const INLINE = /(`[^`\n]+`)|(\*\*[^*\n]+\*\*)|(\[[^\]\n]+\]\((https?:\/\/[^\s)]+)\))|((?<![\w*])\*(?=\S)[^*\n]*?\S\*(?![\w*])|(?<![\w\\])_(?=\S)[^_\n]*?\S_(?!\w))|(https?:\/\/[^\s<>()]+[^\s<>().,;:!?'"])/g;

function inline(text: string): ReactNode[] {
  const out: ReactNode[] = [];
  let last = 0;
  for (const m of text.matchAll(INLINE)) {
    const at = m.index!;
    if (at > last) out.push(text.slice(last, at));
    const [whole, code, bold, link, href, em, url] = m;
    const key = out.length;
    if (code) out.push(<code key={key}>{code.slice(1, -1)}</code>);
    else if (bold) out.push(<strong key={key}>{inline(bold.slice(2, -2))}</strong>);
    else if (link)
      out.push(
        <a key={key} href={href} target="_blank" rel="noreferrer">
          {inline(link.slice(1, link.indexOf("](")))}
        </a>,
      );
    else if (em) out.push(<em key={key}>{inline(em.slice(1, -1))}</em>);
    else if (url)
      out.push(
        <a key={key} href={url} target="_blank" rel="noreferrer">
          {url}
        </a>,
      );
    else out.push(whole);
    last = at + whole.length;
  }
  if (last < text.length) out.push(text.slice(last));
  return out;
}

// Soft line breaks stay visible: replies often use single newlines for layout.
const withBreaks = (lines: string[]) =>
  lines.map((line, i) => (
    <Fragment key={i}>
      {i > 0 && <br />}
      {inline(line)}
    </Fragment>
  ));

const LIST = /^\s*([-*+]|\d+[.)])\s+(.*)$/;
const FENCE = /^\s*(```|~~~)/;
const HEADING = /^#{1,6}\s/;

export function Markdown({ text, className = "" }: { text: string; className?: string }) {
  const lines = text.replace(/\r\n/g, "\n").split("\n");
  const blocks: ReactNode[] = [];
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    const key = blocks.length;
    const fence = line.match(/^\s*(```|~~~)\s*([\w+-]*)/);
    if (fence) {
      const body: string[] = [];
      i++;
      while (i < lines.length && !lines[i].trim().startsWith(fence[1])) body.push(lines[i++]);
      i++;
      blocks.push(
        <pre key={key} className="md-code" data-lang={fence[2] || undefined}>
          <code>{body.join("\n")}</code>
        </pre>,
      );
      continue;
    }
    if (!line.trim()) {
      i++;
      continue;
    }
    const heading = line.match(/^(#{1,6})\s+(.*)$/);
    if (heading) {
      blocks.push(
        <p key={key} className={`md-h md-h${Math.min(heading[1].length, 3)}`}>
          {inline(heading[2])}
        </p>,
      );
      i++;
      continue;
    }
    if (LIST.test(line)) {
      const ordered = /^\s*\d/.test(line);
      const items: string[][] = [];
      // A fence or heading ends the list, so a step's code block stays code.
      while (i < lines.length && lines[i].trim() && !FENCE.test(lines[i]) && !HEADING.test(lines[i])) {
        const item = lines[i].match(LIST);
        if (item) items.push([item[2]]);
        else items.at(-1)!.push(lines[i].trim());
        i++;
      }
      const Tag = ordered ? "ol" : "ul";
      blocks.push(
        <Tag key={key} start={ordered ? parseInt(line, 10) || undefined : undefined}>
          {items.map((item, n) => (
            <li key={n}>{withBreaks(item)}</li>
          ))}
        </Tag>,
      );
      continue;
    }
    if (line.startsWith(">")) {
      const quote: string[] = [];
      while (i < lines.length && lines[i].startsWith(">")) quote.push(lines[i++].replace(/^>\s?/, ""));
      blocks.push(<blockquote key={key}>{withBreaks(quote)}</blockquote>);
      continue;
    }
    const para: string[] = [];
    while (
      i < lines.length &&
      lines[i].trim() &&
      !FENCE.test(lines[i]) &&
      !HEADING.test(lines[i]) &&
      !LIST.test(lines[i]) &&
      !lines[i].startsWith(">")
    )
      para.push(lines[i++]);
    blocks.push(<p key={key}>{withBreaks(para)}</p>);
  }
  return <div className={`md ${className}`.trim()}>{blocks}</div>;
}
