package main

import "testing"

func TestAtoi(t *testing.T) {
	cases := map[string]int{
		"":    -1, // fresh-review regression: "" used to parse as 0
		" ":   -1,
		"08":  8,
		"23":  23,
		"x":   -1,
		"-1":  -1,
		"24":  24, // bounds like hh>23 are the caller's job; atoi only validates digits
		"999": 999,
	}
	for in, want := range cases {
		if got := atoi(in); got != want {
			t.Errorf("atoi(%q) = %d, want %d", in, got, want)
		}
	}
}
