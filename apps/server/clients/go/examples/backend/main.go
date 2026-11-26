// A shop's backend: its key creates a customer's wallet, then signs a
// 15-minute session that may only read that wallet, for the customer's
// browser. The secret never leaves this process.
//
//	HASHINBOX_URL=https://pay.example HASHINBOX_KEY_ID=hip/… HASHINBOX_SECRET=his/… \
//	  go run ./examples/backend -chain ethereum -network mainnet
package main

import (
	"context"
	"flag"
	"fmt"
	"log"
	"os"
	"time"

	"hashinbox"
)

func main() {
	chain := flag.String("chain", "ethereum", "the wallet's chain")
	network := flag.String("network", "mainnet", "the wallet's network")
	customer := flag.String("customer", "customer-1832", "who the session is for")
	flag.Parse()

	secret, err := hashinbox.ParseSecret(os.Getenv("HASHINBOX_KEY_ID"), os.Getenv("HASHINBOX_SECRET"))
	if err != nil {
		log.Fatal(err)
	}
	// The client signs its own short sessions with the key's scopes.
	client := hashinbox.NewClient(os.Getenv("HASHINBOX_URL"), hashinbox.WithSecret(secret))

	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	wallet, err := hashinbox.WalletsCreate(ctx, client, hashinbox.WalletsCreateRequest{
		CreateWalletRequest: hashinbox.CreateWalletRequest{Chain: *chain, Network: *network, ExternalID: customer},
	})
	if err != nil {
		log.Fatal(err)
	}

	read := hashinbox.Fill("com.hashinbox.wallets[{id}].$read", map[string]string{"id": wallet.ID})
	session, err := secret.Issue(
		hashinbox.WithScopes(hashinbox.MustScopes(read)),
		hashinbox.WithSubject(*customer),
		hashinbox.WithTTL(15*time.Minute),
	)
	if err != nil {
		log.Fatal(err)
	}
	fmt.Println("wallet: ", wallet.ID, wallet.Address)
	fmt.Println("expires:", session.ExpiresAt.Format(time.RFC3339))
	fmt.Println("token:  ", session.Token)
}
