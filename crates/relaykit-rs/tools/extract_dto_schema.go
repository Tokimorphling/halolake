package main

import (
	"bytes"
	"encoding/json"
	"go/ast"
	"go/format"
	"go/parser"
	"go/token"
	"os"
	"path/filepath"
	"reflect"
	"strconv"
	"strings"
)

func main() {
	defs := map[string]any{}
	files, _ := filepath.Glob(os.Args[1] + "/*.go")
	for _, path := range files {
		if strings.HasSuffix(path, "_test.go") {
			continue
		}
		f, _ := parser.ParseFile(token.NewFileSet(), path, nil, 0)
		for _, d := range f.Decls {
			gd, ok := d.(*ast.GenDecl)
			if !ok {
				continue
			}
			for _, sp := range gd.Specs {
				ts, ok := sp.(*ast.TypeSpec)
				if !ok {
					continue
				}
				st, ok := ts.Type.(*ast.StructType)
				if !ok {
					continue
				}
				fields := []any{}
				for _, field := range st.Fields.List {
					var b bytes.Buffer
					format.Node(&b, token.NewFileSet(), field.Type)
					tag := ""
					if field.Tag != nil {
						tag, _ = strconv.Unquote(field.Tag.Value)
					}
					jt := reflect.StructTag(tag).Get("json")
					if jt == "-" {
						continue
					}
					if len(field.Names) == 0 {
						if jt != "" {
							fields = append(fields, map[string]any{"name": b.String(), "json": strings.Split(jt, ",")[0], "type": b.String(), "omit": strings.Contains(jt, ",omitempty")})
						} else {
							fields = append(fields, map[string]any{"embed": b.String()})
						}
						continue
					}
					for _, name := range field.Names {
						if !name.IsExported() {
							continue
						}
						jname := strings.Split(jt, ",")[0]
						if jname == "" {
							jname = name.Name
						}
						fields = append(fields, map[string]any{"name": name.Name, "json": jname, "type": b.String(), "omit": strings.Contains(jt, ",omitempty")})
					}
				}
				defs[ts.Name.Name] = fields
			}
		}
	}
	json.NewEncoder(os.Stdout).Encode(defs)
}
