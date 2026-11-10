using System.Collections.Concurrent;

var app = WebApplication.CreateBuilder(args).Build();
var items = new ConcurrentDictionary<int, Item>();
var next = 0;

app.MapGet("/health", () => Results.Ok("ready"));
app.MapGet("/items", () => Results.Ok(items.Values.OrderBy(i => i.Id)));
app.MapPost("/items", (NewItem body) =>
{
    var item = new Item(Interlocked.Increment(ref next), body.Name ?? "", body.Quantity);
    items[item.Id] = item;
    return Results.Created($"/items/{item.Id}", item);
});

app.Run();

record NewItem(string? Name, int Quantity);
record Item(int Id, string Name, int Quantity);
