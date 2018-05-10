// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package symbolize

import (
	"bufio"
	"context"
	"fmt"
	"io"
	"strings"
)

const (
	elemPrefix   string = "{{{"
	elemSuffix   string = "}}}"
	colorPrefix  string = "\033["
	modulePrefix string = "{{{module:"
	mmapPrefix   string = "{{{mmap:"
	pcPrefix     string = "{{{pc:"
	btPrefix     string = "{{{bt:"
)

func findIndex(s ParserState, sub string) int {
	idx := strings.Index(string(s), sub)
	if idx == -1 {
		return len(s)
	}
	return idx
}

func ParseText(b *ParserState) interface{} {
	idx := findIndex(*b, elemPrefix)
	idx2 := findIndex(*b, colorPrefix)
	if idx2 < idx {
		idx = idx2
	}

	if idx == 0 {
		return nil
	}
	text := string((*b)[:idx])
	*b = (*b)[idx:]
	return &Text{text}
}

func ParseBt(b *ParserState) interface{} {
	return b.prefix(btPrefix, func(b *ParserState) interface{} {
		num, err := b.intBefore(":")
		if err != nil {
			return nil
		}
		addr, err := b.intBefore(elemSuffix)
		if err != nil {
			return nil
		}
		return &BacktraceElement{num: num, vaddr: addr}
	})
}

func ParsePc(b *ParserState) interface{} {
	return b.prefix(pcPrefix, func(b *ParserState) interface{} {
		addr, err := b.intBefore(elemSuffix)
		if err != nil {
			return nil
		}
		return &PCElement{vaddr: addr}
	})
}

func ParseColor(b *ParserState) interface{} {
	return b.prefix(colorPrefix, func(b *ParserState) interface{} {
		var out ColorGroup
		var err error
		if out.color, err = b.intBefore("m"); err != nil {
			return nil
		}
		b.many(&out.children, func(b *ParserState) interface{} {
			return b.choice(ParsePc, ParseText)
		})
		return &out
	})
}

func ParseModule(b *ParserState) interface{} {
	return b.prefix(modulePrefix, func(b *ParserState) interface{} {
		var out Module
		var err error
		if out.id, err = b.intBefore(":"); err != nil {
			return nil
		}
		if out.name, err = b.before(":"); err != nil {
			return nil
		}
		if !b.expect("elf:") {
			return nil
		}
		if out.build, err = b.before(elemSuffix); err != nil {
			return nil
		}
		return &ModuleElement{out}
	})
}

func ParseMapping(b *ParserState) interface{} {
	return b.prefix(mmapPrefix, func(b *ParserState) interface{} {
		var out Segment
		var err error
		if out.vaddr, err = b.intBefore(":"); err != nil {
			return nil
		}
		if out.size, err = b.intBefore(":"); err != nil {
			return nil
		}
		if b.expect("load:") {
			if out.mod, err = b.intBefore(":"); err != nil {
				return nil
			}
			if out.flags, err = b.before(":"); err != nil {
				return nil
			}
			if out.modRelAddr, err = b.intBefore(elemSuffix); err != nil {
				return nil
			}
		} else {
			if _, err = b.before("}}}"); err != nil {
				return nil
			}
		}
		return &MappingElement{out}
	})
}

func ParseReset(b *ParserState) interface{} {
	if b.expect("{{{reset}}}") {
		return &ResetElement{}
	}
	return nil
}

func ParsePresentationGroup(b *ParserState) interface{} {
	var p PresentationGroup
	b.many(&p.children, func(b *ParserState) interface{} {
		return b.choice(ParseColor, ParseModule, ParseMapping, ParseBt, ParseReset, ParsePc, ParseText)
	})
	return &p
}

func ParseLine(line string) Node {
	buf := ParserState(line)
	b := &buf
	out := ParsePresentationGroup(b)
	if len(buf) != 0 || out == nil {
		return nil
	}
	return out.(Node)
	// TODO: add node type for malformed text and emit warning
}

func ParseLogLine(b *ParserState) (InputLine, error) {
	var out InputLine
	if b.expect("[") {
		var err error
		time, err := b.floatBefore("]")
		if err != nil {
			return out, err
		}
		b.whitespace()
		pid, err := b.decBefore(".")
		if err != nil {
			return out, err
		}
		out.source = process(pid)
		tid, err := b.decBefore(">")
		if err != nil {
			return out, err
		}
		out.header = logHeader{process: pid, thread: tid, time: time}
		b.whitespace()
		out.msg = string(*b)
		return out, nil
	}
	return out, fmt.Errorf("malformed serial log line")
}

func StartParsing(ctx context.Context, reader io.Reader) <-chan InputLine {
	out := make(chan InputLine)
	go func() {
		defer close(out)
		scanner := bufio.NewScanner(reader)
		for scanner.Scan() {
			select {
			case <-ctx.Done():
				return
			default: // Continue.
			}
			text := ParserState(scanner.Text())
			b := &text
			// If there is some stuff at the start, make it into a dummy line.
			dummyText, err := b.onlyBefore("[")
			if err == nil && dummyText != "" {
				var line InputLine
				line.source = dummySource{}
				line.msg = dummyText
				out <- line
			}
			// Now attempt to parse the log line
			line, err := ParseLogLine(b)
			// If we error out send this line whole to the dummy process
			if err != nil {
				// Some partial bit of this might have parsed so just modify it.
				line.source = dummySource{}
				line.msg = string(*b)
			}
			out <- line
		}
	}()
	return out
}
