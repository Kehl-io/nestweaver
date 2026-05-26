package main

import (
	"fmt"
	"strings"
)

type Greeter interface {
	Greet(name string) string
	Farewell(name string) string
}

type ConsoleGreeter struct {
	Prefix string
}

func (g *ConsoleGreeter) Greet(name string) string {
	return fmt.Sprintf("%s Hello, %s!", g.Prefix, name)
}

func (g *ConsoleGreeter) Farewell(name string) string {
	return fmt.Sprintf("%s Goodbye, %s!", g.Prefix, name)
}

func NewGreeter(prefix string) Greeter {
	return &ConsoleGreeter{Prefix: strings.TrimSpace(prefix)}
}

func main() {
	greeter := NewGreeter(">>>")
	result := greeter.Greet("world")
	fmt.Println(result)
}
