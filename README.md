# Video file downloader

## Usage

```console
$ moget --help
Video file downloader

Usage: moget [OPTIONS] <URL>

Arguments:
  <URL>  URL of the movie file to download

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
          Maximum amount of transfers to do simultaneously for each stream [default: 4]
  -h, --help
          Print help information
  -V, --version
          Print version information
```
