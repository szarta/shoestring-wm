// Command example is a tiny demo of the shoestring-wm Go client.
//
// Run inside a shoestring-wm session (so $SHOESTRING_WM_SOCKET is set):
//
//	cd clients/go && go run ./example
package main

import (
	"fmt"
	"io"
	"log"

	shoestringipc "github.com/szarta/shoestring-wm/clients/go"
)

func main() {
	wm, err := shoestringipc.New()
	if err != nil {
		log.Fatal(err)
	}

	ws, err := wm.Workspaces()
	if err != nil {
		log.Fatal(err)
	}
	fmt.Printf("active workspace: %v of %v\n", ws["active"], ws["count"])

	resp, err := wm.Windows()
	if err != nil {
		log.Fatal(err)
	}
	windows, _ := resp["windows"].([]any)
	fmt.Printf("%d window(s):\n", len(windows))
	for _, w := range windows {
		win, _ := w.(map[string]any)
		fmt.Printf("  [%v] %v: %v\n", win["workspace"], win["app_id"], win["title"])
	}

	fmt.Println("watching events (Ctrl-C to stop)...")
	stream, err := wm.Events()
	if err != nil {
		log.Fatal(err)
	}
	defer stream.Close()
	for {
		ev, err := stream.Next()
		if err == io.EOF {
			break
		}
		if err != nil {
			log.Fatal(err)
		}
		fmt.Printf("  event: %v\n", ev)
	}
}
