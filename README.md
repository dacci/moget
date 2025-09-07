# Video file downloader

## Usage

```console
$ moget --help
Video file downloader

Usage: moget [OPTIONS] [URL]

Arguments:
  [URL]  URL of the movie file to download

Options:
      --protocol <PROTOCOL>
          Protocol to use to communicate with the server [default: auto] [possible values: auto, vimeo, hls]
  -o, --output <FILE>
          Write output to FILE
  -H, --header <X-Name: value>
          Extra header to include in the request
      --compressed
          For compatibility with cURL, ignored
      --connect-timeout <fractional seconds>
          Maximum time in seconds that you allow connection to take [default: 10]
  -m, --max-time <fractional seconds>
          Maximum time in seconds that you allow single download to take [default: 60]
      --retry <num>
          Set the maximum number of allowed retries attempts [default: 10]
      --parallel-max <num>
          Maximum amount of transfers to do simultaneously for each stream [default: 1]
  -x, --proxy <[protocol://]host[:port]>
          Use the specified proxy
  -U, --proxy-user <user:password>
          Specify the user name and password to use for proxy authentication
  -b, --cookie <data>
          Pass the exact data to send to the HTTP server in the Cookie header
  -s, --seek <position>
          Discard input until the timestamps reach position
  -f, --fast-start
          Run a second pass moving the index (moov atom) to the beginning of the file
      --worst
          Choose the lowest quality stream when multiple streams are found
      --skip-bytes <LEN>
          Skip LEN bytes from the beginning of each segment
      --generate-completion <SHELL>
          Generate shell completions [possible values: bash, elvish, fish, powershell, zsh]
  -h, --help
          Print help
  -V, --version
          Print version
```
