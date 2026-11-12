using System;
using System.Collections.Generic;
using System.IO;
using System.Net.Http;
using System.Text;
using System.Text.Json;
using System.Threading;
using System.Threading.Tasks;

namespace EMLst
{
    internal class MessageChecker
    {
        private static readonly HttpClient httpClient = new HttpClient();
        private string token;
        private int timestamp;
        private string baseurl;
        public static string filePath = "input.txt";
        public string basepath { get; set; }
        private CancellationTokenSource cancellationTokenSource;

        public MessageChecker(string token,string baseurl,string basepath)
        {
            this.token = token;
            this.timestamp = 0;
            this.baseurl = baseurl;
            this.basepath = basepath; 
        }

        public void StartMonitoring()
        {
            // Initialize the cancellation token source
            cancellationTokenSource = new CancellationTokenSource();

            clearSyncFile();
            // Run the monitoring task on a separate thread
            Task.Run(() => CheckMessages(cancellationTokenSource.Token), cancellationTokenSource.Token);
        }

        // Do not truncate the sync file here. A running game holds a byte offset
        // into it and would seek past the end afterwards, so every command we
        // write from then on is lost without any error on either side. Only the
        // handshake (LogFileMonitor, case 'i') may clear it, because that is the
        // one moment the game starts reading at zero again.
        public void clearSyncFile()
        {
            _ = WriteMessagesToFileAsync([], basepath, MessageChecker.filePath);
        }

        public void StopMonitoring()
        {
            // Cancel the monitoring task
            cancellationTokenSource.Cancel();
        }

        public async Task CheckMessages(CancellationToken cancellationToken)
        {
            while (!cancellationToken.IsCancellationRequested) // This loop can be controlled externally, e.g., with a cancellation token
            {
                System.Diagnostics.Debug.WriteLine($"Try get Message:");
                try
                {
                    object jsonResponse = await MainWindow.RequestAsync($"{baseurl}?action=commands_pending&session_token={token}", null, "GET");

                    if (jsonResponse != null && jsonResponse is Dictionary<string, object> dictionary && dictionary.ContainsKey("commands"))
                    {
                        List<object> commands = ((JsonElement)dictionary["commands"]).Deserialize<List<object>>();
                        List<string> acknowledge = new List<string>();
                        List<string> messages = new List<string>();

                        foreach (object com in commands)
                        {

                            Dictionary<string, object> command = ((JsonElement)com).Deserialize<Dictionary<string, object>>();
                            Dictionary<string, object> payload = JsonSerializer.Deserialize<Dictionary<string, object>>(command["payload"].ToString());
                            var event_id = payload.ContainsKey("event_game_id") && payload["event_game_id"]!=null ? payload["event_game_id"] :
                                            (payload.ContainsKey("event_id") && payload["event_id"] != null ? payload["event_id"] : "-1");
                            string message = "";
                            if (command["type"].ToString().Equals("assign"))
                            {
                                string player_id = "-1";
                                if (payload["assign_to_player_id"]!=null && payload["assign_to_player_id"].ToString().Length > 0)
                                {
                                    player_id = payload["assign_to_player_id"].ToString();
                                    //Create Message
                                }
                                string mode = payload.ContainsKey("mode")?payload["mode"].ToString():"-1";
                                System.Diagnostics.Debug.WriteLine(command["payload"].ToString());
                                Dictionary<string, object> target = ((JsonElement)payload["target"]).Deserialize<Dictionary<string, object>>();
                                message = $"203|{payload["game_vehicle_id"]}|{event_id}|{target["x"]}|{target["y"]}|{player_id}|{mode}|";
                            }else if (command["type"].ToString().Equals("unassign"))
                            {
                                message = $"204|{payload["game_vehicle_id"]}|";
                            }else if (command["type"].ToString().Equals("event_create"))
                            {
                                Dictionary<string, object> target = ((JsonElement)payload["target"]).Deserialize<Dictionary<string, object>>();
                                message = $"110|{payload["event_id"]}|{target["x"]}|{target["y"]}|{payload["name"]}|";
                            }
                            else if (command["type"].ToString().Equals("event_delete"))
                            {
                                message = $"112|{payload["event_id"]}|{payload["event_game_id"]}|";
                            }
                            else if (command["type"].ToString().Equals("message_create"))
                            {
                                message = $"311|{event_id}|{payload["short_message"]}|{payload["long_message"]}|";
                            }
                            else if (command["type"].ToString().Equals("message_revoke"))
                            {
                                message = $"312|{event_id}|{payload["short_message"]}|";
                            }

                            if (message.Length > 0)
                            {
                                messages.Add(message);
                                acknowledge.Add(command["id"].ToString());
                            }
                        }
                        if (messages.Count > 0)
                            {
                            // Write messages to file
                            await WriteMessagesToFileAsync(messages, basepath, filePath);
                            Dictionary<string, object> data = new Dictionary<string,object>();
                            data["session_token"] = token;
                            data["command_ids"] = acknowledge;
                            await MainWindow.RequestAsync($"{baseurl}?action=commands_ack", data, "POST");
                        }
                    }
                }
                catch (HttpRequestException e)
                {
                    Console.WriteLine($"Request error: {e.Message}");
                    App.log($"Message Checker: A request error occurred: {e.Message}");
                    // Handle request errors, possibly retry after a delay
                }
                catch (JsonException e)
                {
                    Console.WriteLine($"JSON parsing error: {e.Message}");
                    App.log($"Message Checker: JSON parsing error: {e.Message}");
                    // Handle JSON parsing errors
                }
                catch (Exception e)
                {
                    Console.WriteLine($"Unexpected error: {e.Message}");
                    App.log($"Message Checker: Unexpected error: {e.Message}");
                }

                // Wait before retrying, or implement a retry mechanism
                await Task.Delay(5000); // Wait 5 seconds before the next attempt
            }
        }

        public static async Task WriteMessagesToFileAsync(List<string> messages, string basepath, string filePath)
        {
            var path = Path.Combine(basepath, filePath);
            bool written = false;

            while (!written)
            {
                try
                {
                    // Ensure we don't start appending with two empty lines at the end already.
                    TrimTerminalMarker(path);

                    using (var writer = new StreamWriter(path, append: true, encoding: new UTF8Encoding(encoderShouldEmitUTF8Identifier: false)))//
                    {
                        foreach (var message in messages)
                        {
                            await writer.WriteLineAsync(message);
                        }

                        // Leave exactly one *blank* line at EOF.
                        await writer.WriteLineAsync("+");// equivalent to WriteLineAsync("")
                    }

                    written = true;
                }
                catch (IOException)
                {
                    Console.WriteLine("File is not available, retrying...");
                    await Task.Delay(100);
                }
            }
        }

        /// <summary>
        /// Cut off the trailing "+" line, the marker the game stops reading at, so the
        /// next commands land exactly where it stopped. Takes the marker with or
        /// without a line break around it, in any flavour: the game reads the file in
        /// text mode and only looks at the "+", and a marker we fail to cut blocks it
        /// for the rest of the round.
        /// </summary>
        private static void TrimTerminalMarker(string path)
        {
            if (!File.Exists(path))
                return;

            using (var fs = new FileStream(path, FileMode.Open, FileAccess.ReadWrite, FileShare.Read))
            {
                var window = (int)Math.Min(fs.Length, 4); // at most "\n+\r\n"
                if (window == 0)
                    return;

                fs.Seek(-window, SeekOrigin.End);
                var tail = new byte[window];
                if (fs.Read(tail, 0, window) != window)
                    return;

                var end = window;
                if (tail[end - 1] == (byte)'\n') end--;
                if (end > 0 && tail[end - 1] == (byte)'\r') end--;
                if (end == 0 || tail[end - 1] != (byte)'+')
                    return;

                long markerStart = fs.Length - window + end - 1;
                // A data line ending in "+" is no marker.
                if (markerStart > 0 && tail[end - 2] != (byte)'\n')
                    return;

                fs.SetLength(markerStart);
            }
        }
    }
}
