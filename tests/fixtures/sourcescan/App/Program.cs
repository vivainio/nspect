using System;
using System.Collections.Generic;
using Serilog;
using Newtonsoft.Json;

namespace App;

public class Program
{
    public static void Main()
    {
        Log.Information("hi");
        var data = JsonConvert.SerializeObject(new { x = 1 });
        Console.WriteLine(data);
    }
}
